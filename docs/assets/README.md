# Brand assets

Paired with [fakescan](https://github.com/b1tninja/fakescan): device art, a two-letter monogram **Fp**, and a banner wordmark. The root README shows this logo at width **380** (viewBox 760×240), the same frame as fakescan.

| File | Use |
|------|-----|
| [`fakeprint-logo.svg`](fakeprint-logo.svg) | Dark banner: beige printer + **Fp** + wordmark (corners transparent) |
| [`fakeprint-mark.svg`](fakeprint-mark.svg) | App icon: the same printer on a rounded plate (LCD **PC LOAD LETTER**) |
| [`fakeprint.ico`](fakeprint.ico) | Windows `fakeprint.exe` (multi-size) |
| [`fakeprint.png`](fakeprint.png) | 256×256 raster of the mark |
| [`render_icons.py`](render_icons.py) | Rebuild `.ico` / `.png` from the mark |

Charcoal banner `#1c1f1c`, cream chassis `#eee6d6` → `#c9beaa`, paper `#fffcf7`, LCD phosphor `#7CFF7C`, LED `#ff5314`, wordmark `#f3efe4`. Easter egg: the printer LCD reads **PC LOAD LETTER** ([Office Space](https://en.wikipedia.org/wiki/Office_Space)).

```bash
python docs/assets/render_icons.py
```
