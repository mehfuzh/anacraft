#!/usr/bin/env python3
"""Draw the OAuth consent logo: a 16x16 mark in the osaka-jade palette,
upscaled nearest-neighbour so the pixels stay square.

Drawn from scratch — Mojang's own item textures are proprietary and cannot
ship here or pass Google's OAuth branding review.

Set MARK to pick a glyph. Both grids use the same shading letters, so either
can be hand-edited as ASCII.

    python3 scripts/gen-logo.py

Writes assets/oauth-logo-{120,512}.png (Google's OAuth consent screen),
assets/icon-128.png (the icon `craft mcp` hands an MCP client) and
docs/favicon.svg (the site tab icon), so all three come from this one grid.
"""

import hashlib
import os
import re

from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# src/theme.rs, OSAKA_JADE
INK = (9, 16, 13)
PALETTE = {
    ".": None,
    "L": (126, 240, 208),  # bright  — diamond highlight
    "D": (45, 213, 183),   # accent  — diamond body
    "S": (28, 138, 117),   # deep    — diamond shadow
    "l": (226, 176, 132),  # clay light — handle highlight
    "w": (199, 137, 92),   # clay       — handle body
    "s": (132, 85, 53),    # clay dark  — handle shadow
}

PICKAXE = [
    "................",
    ".....LLLLLL.....",
    "...LLDDDDDDLL...",
    "..LDD.DLLD.DDL..",
    ".LDD..DLLD..DDL.",
    ".DDS..DDDD..SDD.",
    ".SS...DSSD...SS.",
    "......lwws......",
    "......lwws......",
    "......lwws......",
    "......lwws......",
    "......lwws......",
    "......lwws......",
    "......lwws......",
    "......swws......",
    "................",
]

# Original A: a heavy symmetric wedge — solid through the apex, a small
# triangular counter, four-cell strokes, and a bar low enough to leave the
# legs open beneath it.
#
# Anaplan's mark is the same species and an earlier revision here traced it.
# This one is symmetric on its own proportions; theirs is asymmetric, with
# the right leg swallowed by the bottom wedge. That asymmetry is the part
# that is theirs, so it is the part this does not borrow.
LETTER_A = [
    "................",
    "......DDDD......",
    ".....DDDDDD.....",
    "....DDDDDDDD....",
    "....DDDDDDDD....",
    "...DDDD..DDDD...",
    "...DDDD..DDDD...",
    "..DDDD....DDDD..",
    "..DDDD....DDDD..",
    ".DDDD......DDDD.",
    ".DDDDDDDDDDDDDD.",
    ".DDDDDDDDDDDDDD.",
    ".DDDD......DDDD.",
    ".DDDD......DDDD.",
    ".DDDD......DDDD.",
    "................",
]

MARK = LETTER_A


def rgb(colour):
    return "#%02x%02x%02x" % colour


def runs(row):
    """Yield (start, length, colour) for each horizontal run of one colour."""
    x = 0
    while x < len(row):
        colour = PALETTE[row[x]]
        end = x
        while end + 1 < len(row) and PALETTE[row[end + 1]] == colour:
            end += 1
        if colour:
            yield x, end - x + 1, colour
        x = end + 1


def write_svg():
    """One <rect> per run — the pixels stay crisp at any tab size."""
    parts = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" '
        'shape-rendering="crispEdges">',
        f'<rect width="16" height="16" fill="{rgb(INK)}"/>',
    ]
    for y, row in enumerate(MARK):
        for x, run, colour in runs(row):
            parts.append(
                f'<rect x="{x}" y="{y}" width="{run}" height="1" '
                f'fill="{rgb(colour)}"/>'
            )
    parts.append("</svg>")

    out = os.path.join(ROOT, "docs", "favicon.svg")
    with open(out, "w") as fh:
        fh.write("".join(parts))
    print(f"wrote {out}")


def write_mark():
    """Transparent, tightly-cropped variant for the site header wordmark."""
    xs = [x for row in MARK for x, ch in enumerate(row) if ch != "."]
    ys = [y for y, row in enumerate(MARK) if row.strip(".")]
    x0, x1, y0, y1 = min(xs), max(xs), min(ys), max(ys)

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" '
        f'viewBox="{x0} {y0} {x1 - x0 + 1} {y1 - y0 + 1}" '
        f'shape-rendering="crispEdges">'
    ]
    for y, row in enumerate(MARK):
        for x, run, colour in runs(row):
            parts.append(
                f'<rect x="{x}" y="{y}" width="{run}" height="1" '
                f'fill="{rgb(colour)}"/>'
            )
    parts.append("</svg>")

    out = os.path.join(ROOT, "docs", "mark.svg")
    with open(out, "w") as fh:
        fh.write("".join(parts))
    print(f"wrote {out}")


PAGES = ("index.html", "_of.html", "privacy.html", "terms.html")


def stamp_pages():
    """Point the pages at ?v=<content hash> of each asset.

    GitHub Pages serves these with max-age=14400, so without a changing URL a
    redrawn mark sits stale in browsers for four hours while the favicon, which
    the browser refetches on its own schedule, updates — the two then disagree.
    """
    stamps = {}
    for name in ("favicon.svg", "mark.svg"):
        blob = open(os.path.join(ROOT, "docs", name), "rb").read()
        stamps[name] = hashlib.sha256(blob).hexdigest()[:8]

    for page in PAGES:
        path = os.path.join(ROOT, "docs", page)
        text = open(path).read()
        before = text
        for name, digest in stamps.items():
            text = re.sub(
                rf'({re.escape(name)})(\?v=[0-9a-f]+)?(")',
                rf'\g<1>?v={digest}\g<3>',
                text,
            )
        if text != before:
            open(path, "w").write(text)
            print(f"stamped {page}")


def main():
    img = Image.new("RGB", (16, 16), INK)
    px = img.load()
    for y, row in enumerate(MARK):
        for x, ch in enumerate(row):
            colour = PALETTE[ch]
            if colour:
                px[x, y] = colour

    for size in (120, 512):
        out = os.path.join(ROOT, "assets", f"oauth-logo-{size}.png")
        img.resize((size, size), Image.NEAREST).save(out)
        print(f"wrote {out}")

    # The MCP handshake carries this one inline, as a base64 data URI, so it is
    # kept small: a client draws it at list-row size, and every byte here is a
    # byte on the wire of every `initialize`.
    out = os.path.join(ROOT, "assets", "icon-128.png")
    img.resize((128, 128), Image.NEAREST).save(out, optimize=True)
    print(f"wrote {out}")

    write_svg()
    write_mark()
    stamp_pages()


if __name__ == "__main__":
    main()
