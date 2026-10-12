# Brand assets

| File | Use |
|------|-----|
| [`fakeprint-logo.svg`](fakeprint-logo.svg) | Monogram **Fp** + wordmark (README, docs) |
| [`fakeprint-mark.svg`](fakeprint-mark.svg) | **Fp** squircle (avatars, favicons) |
| [`fakeprint.ico`](fakeprint.ico) | Windows `fakeprint.exe` icon (multi-size) |
| [`fakeprint.png`](fakeprint.png) | 256×256 raster of the mark |
| [`render_icons.py`](render_icons.py) | Rebuild `.ico` / `.png` from the monogram |

Colors: deep teal `#115e59` for the mark, accent `#0d9488` (matches fakescan’s family). Charcoal wordmark `#1a1f26` (lightens under `prefers-color-scheme: dark`).

Easter egg: the logo wordmark carries a small **PC LOAD LETTER** line ([Office Space](https://en.wikipedia.org/wiki/Office_Space)).

Regenerate rasters after mark changes:

```bash
python docs/assets/render_icons.py
```
