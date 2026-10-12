"""Rasterize the Fp monogram into multi-size .ico (and a PNG).

Run from repo root or this directory:
  python docs/assets/render_icons.py
"""

from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

HERE = Path(__file__).resolve().parent
SIZES = (16, 24, 32, 48, 64, 128, 256)


def _font(size: int) -> ImageFont.ImageFont:
    windir = Path(r"C:\Windows\Fonts")
    for name in ("segoeuib.ttf", "arialbd.ttf", "courbd.ttf"):
        path = windir / name
        if path.is_file():
            return ImageFont.truetype(str(path), size)
    for name in ("segoeuib.ttf", "arialbd.ttf", "DejaVuSans-Bold.ttf"):
        try:
            return ImageFont.truetype(name, size)
        except OSError:
            continue
    return ImageFont.load_default()


def render_mark(letters: str, fill: str, size: int) -> Image.Image:
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    radius = max(2, int(size * 0.22))
    draw.rounded_rectangle((0, 0, size - 1, size - 1), radius=radius, fill=fill)
    font = _font(max(8, int(size * 0.52)))
    bbox = draw.textbbox((0, 0), letters, font=font)
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    x = (size - tw) / 2 - bbox[0]
    y = (size - th) / 2 - bbox[1] - size * 0.02
    draw.text((x, y), letters, font=font, fill="#ffffff")
    return img


def write_ico(letters: str, fill: str, dest: Path) -> None:
    frames = [render_mark(letters, fill, s).convert("RGBA") for s in SIZES]
    largest = frames[-1]
    largest.save(dest, format="ICO", sizes=[(s, s) for s in SIZES], append_images=frames[:-1])
    png = dest.with_suffix(".png")
    largest.save(png, format="PNG")
    print(f"wrote {dest} ({', '.join(str(s) for s in SIZES)}) and {png.name}")


def main() -> None:
    write_ico("Fp", "#115e59", HERE / "fakeprint.ico")


if __name__ == "__main__":
    main()
