#!/usr/bin/env python3
"""Splice `anacraft capture` output into the site, reading the HTML on stdin.

The captures sit between two markers so regenerating them is a replace rather
than a hand-edit. They used to be maintained by hand, which is how the page came
to advertise a panel the dashboard had stopped drawing. Run it through
`make capture`, which builds the binary first.
"""

import pathlib
import sys

START = "<!-- capture:start -->"
END = "<!-- capture:end -->"

root = pathlib.Path(__file__).resolve().parent.parent
page = root / "docs" / "index.html"

captures = sys.stdin.read().strip()
if not captures:
    sys.exit("nothing on stdin — did `anacraft capture` fail?")

text = page.read_text()
if START not in text or END not in text:
    sys.exit(f"{page} has no {START} / {END} markers")

head, rest = text.split(START, 1)
_, tail = rest.split(END, 1)
page.write_text(f"{head}{START}\n{captures}\n{END}{tail}")

print(f"spliced {len(captures):,} bytes into {page.relative_to(root)}")
