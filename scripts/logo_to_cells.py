#!/usr/bin/env python3
"""Render the MØBIUS logo (round badge PNG) as terminal cells for the TUI.

usage: logo_to_cells.py assets/mobius-logo.png [--cols 32] > crates/tui/assets/logo.cells

Each cell is a 2×2 quadrant-block glyph (▘▝▀▖▌▞▛▗▚▐▜▄▙▟█) with its own
foreground and background tone, chosen to minimise the error against the 2×2
image pixels it covers (a cell is ~1:2, so `cols` × `cols/2` cells is square).
Transparent pixels stay transparent (the TUI keeps whatever background is
under the logo). No image protocol is needed.

output: first line `ink=RRGGBB paper=RRGGBB` (the logo's two colours); then one
line per row, each cell = glyph + fg tone (2 hex) + bg tone (2 hex, `..` =
transparent). Tone 00 = ink, ff = paper.
requires: Pillow
"""
import sys

from PIL import Image, ImageFilter

GLYPHS = " ▘▝▀▖▌▞▛▗▚▐▜▄▙▟█"  # index = TL | TR<<1 | BL<<2 | BR<<3


def arg(name, default, cast):
    if name in sys.argv:
        return cast(sys.argv[sys.argv.index(name) + 1])
    return default


def median(xs):
    xs = sorted(xs)
    return xs[len(xs) // 2]


def main():
    path = sys.argv[1]
    cols = arg("--cols", 32, int)
    rows = cols // 2
    src = Image.open(path).convert("RGBA")
    alpha = src.getchannel("A")
    bbox = alpha.point(lambda v: 255 if v > 128 else 0).getbbox() or (0, 0, src.width, src.height)
    x0, y0, x1, y1 = bbox
    side = max(x1 - x0, y1 - y0)
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    box = (int(cx - side / 2), int(cy - side / 2), int(cx + side / 2), int(cy + side / 2))
    src, alpha = src.crop(box), alpha.crop(box)

    # the logo's two colours: medians of the dark and light opaque pixels
    small = src.resize((256, 256), Image.NEAREST)
    rgb = [p for p in (small.getpixel((x, y)) for y in range(256) for x in range(256)) if p[3] > 250]
    luma = lambda p: (p[0] * 299 + p[1] * 587 + p[2] * 114) // 1000
    ink = [median([p[i] for p in rgb if luma(p) < 80]) for i in range(3)]
    paper = [median([p[i] for p in rgb if luma(p) > 180]) for i in range(3)]
    lo, hi = luma(ink), luma(paper)

    w, h = cols * 2, rows * 2
    grey = Image.alpha_composite(Image.new("RGBA", src.size, (*ink, 255)), src).convert("L")
    grey = grey.resize((w, h), Image.LANCZOS).filter(ImageFilter.UnsharpMask(radius=1, percent=80, threshold=2))
    alpha = alpha.resize((w, h), Image.BOX)
    tone = lambda v: max(0, min(255, round((v - lo) * 255 / (hi - lo))))

    print(f"ink={bytes(ink).hex()} paper={bytes(paper).hex()}")
    for r in range(rows):
        line = []
        for c in range(cols):
            pos = [(c * 2 + dx, r * 2 + dy) for dy in range(2) for dx in range(2)]  # TL TR BL BR
            px = [tone(grey.getpixel(p)) for p in pos]
            clear = [alpha.getpixel(p) < 128 for p in pos]
            best = None
            for m in range(16):
                fg = [px[i] for i in range(4) if m >> i & 1]
                bg = [px[i] for i in range(4) if not m >> i & 1]
                if any(clear[i] for i in range(4) if m >> i & 1):
                    continue  # transparent pixels can only be background
                bgo = [v for i, v in enumerate(px) if not m >> i & 1 and not clear[i]]
                if any(clear) and bgo:
                    continue  # a cell with transparency has one opaque tone
                fm = sum(fg) / len(fg) if fg else 0
                bm = sum(bg) / len(bg) if bg else 0
                err = sum((v - fm) ** 2 for v in fg) + sum((v - bm) ** 2 for v in bg)
                if best is None or err < best[0]:
                    best = (err, m, round(fm), round(bm))
            _, m, fm, bm = best
            line.append(f"{GLYPHS[m]}{fm:02x}{'..' if any(clear) else f'{bm:02x}'}")
        print("".join(line))


if __name__ == "__main__":
    main()
