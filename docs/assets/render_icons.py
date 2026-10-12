"""Rasterize fakeprint-mark.svg into multi-size .ico / .png.

Cairo is used when it is installed. Otherwise a local Chrome or Edge
headless screenshot is used. The SVG stays the drawing.
"""

from __future__ import annotations

import io
import shutil
import subprocess
import tempfile
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
SOURCE = HERE / "fakeprint-mark.svg"
SIZES = (16, 24, 32, 48, 64, 128, 256)


def _via_cairo() -> bytes | None:
    try:
        import cairosvg
    except (ImportError, OSError):
        return None
    return cairosvg.svg2png(url=str(SOURCE), output_width=512, output_height=512)


def _browser() -> Path | None:
    candidates = [
        shutil.which("chrome"),
        shutil.which("msedge"),
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ]
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return Path(candidate)
    return None


def _via_browser() -> bytes:
    browser = _browser()
    if browser is None:
        raise SystemExit("neither cairo nor Chrome/Edge is available to rasterize the mark")
    svg = SOURCE.read_text(encoding="utf-8")
    html = (
        "<!doctype html><html><head><meta charset='utf-8'><style>"
        "html,body{margin:0;padding:0;width:512px;height:512px;background:transparent;overflow:hidden}"
        "svg{display:block;width:512px;height:512px}"
        f"</style></head><body>{svg}</body></html>"
    )
    with tempfile.TemporaryDirectory() as tmp:
        folder = Path(tmp)
        page = folder / "mark.html"
        shot = folder / "mark.png"
        page.write_text(html, encoding="utf-8")
        subprocess.run(
            [
                str(browser),
                "--headless=new",
                "--disable-gpu",
                "--hide-scrollbars",
                "--default-background-color=00000000",
                "--window-size=512,512",
                f"--screenshot={shot}",
                page.as_uri(),
            ],
            check=True,
            capture_output=True,
        )
        return shot.read_bytes()


def main() -> None:
    png = _via_cairo()
    if png is None:
        png = _via_browser()
    master = Image.open(io.BytesIO(png)).convert("RGBA")
    frames = [master.resize((size, size), Image.Resampling.LANCZOS) for size in SIZES]
    dest = HERE / "fakeprint.ico"
    largest = frames[-1]
    largest.save(dest, format="ICO", sizes=[(size, size) for size in SIZES], append_images=frames[:-1])
    largest.save(HERE / "fakeprint.png", format="PNG")
    print(f"wrote {dest} and fakeprint.png from {SOURCE.name}")


if __name__ == "__main__":
    main()
