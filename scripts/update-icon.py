#!/usr/bin/env python3
"""Process assets/goop_icon_full.png into a 128x128 RGBA icon and update
all data-URI references in assets/index.html.

The source is expected to already have a proper alpha background.

Usage:  ./scripts/update-icon.py

Requires: Pillow, numpy
"""

import base64
import io
import os
import re
import sys

from PIL import Image
import numpy as np

PROJECT_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FULL_ICON = os.path.join(PROJECT_DIR, "assets", "goop_icon_full.png")
OUT_PNG = os.path.join(PROJECT_DIR, "assets", "goop_icon.png")
INDEX_HTML = os.path.join(PROJECT_DIR, "assets", "index.html")


def die(msg: str) -> None:
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


# ── load ──────────────────────────────────────────────────────────
if not os.path.isfile(FULL_ICON):
    die(f"{FULL_ICON} not found")

img = Image.open(FULL_ICON)
if img.mode != "RGBA":
    img = img.convert("RGBA")
arr = np.array(img)
h, w = arr.shape[:2]
print(f"Loaded: {w}x{h} ({img.mode})")

# ── crop to non-transparent content ───────────────────────────────
alpha = arr[:, :, 3]
rows = np.any(alpha > 0, axis=1)
cols = np.any(alpha > 0, axis=0)

if not rows.any():
    die("no non-transparent content found")

rmin, rmax = np.where(rows)[0][[0, -1]]
cmin, cmax = np.where(cols)[0][[0, -1]]

# Make a square crop centered on the content, with margin on all sides.
margin = 3
content_h = rmax - rmin + 1
content_w = cmax - cmin + 1
side = max(content_h, content_w) + 2 * margin

# Center the content in the square, clamped to image bounds.
r_center = (rmin + rmax) // 2
c_center = (cmin + cmax) // 2

rmin = max(0, r_center - side // 2)
rmax = min(h - 1, rmin + side - 1)
rmin = max(0, rmax - side + 1)  # re-clamp if pushed off the bottom

cmin = max(0, c_center - side // 2)
cmax = min(w - 1, cmin + side - 1)
cmin = max(0, cmax - side + 1)  # re-clamp if pushed off the right

cropped = arr[rmin : rmax + 1, cmin : cmax + 1]
print(
    f"Content: {content_w}x{content_h}, square side: {side}"
)
print(
    f"Cropped: {cropped.shape[1]}x{cropped.shape[0]} "
    f"(rows {rmin}-{rmax}, cols {cmin}-{cmax})"
)

# ── resize & save icon ────────────────────────────────────────────
img_out = Image.fromarray(cropped, "RGBA")
img_out = img_out.resize((128, 128), Image.LANCZOS)
img_out.save(OUT_PNG, "PNG")
print(f"Saved {OUT_PNG}")

# ── generate base64 for HTML ──────────────────────────────────────
buf = io.BytesIO()
img_out.save(buf, format="PNG")
new_b64 = base64.b64encode(buf.getvalue()).decode("ascii")
print(f"Base64: {len(new_b64)} bytes")

a = np.array(img_out.getchannel("A"))
print(
    f"Alpha: {(a == 0).sum()} transparent, "
    f"{((a > 0) & (a < 255)).sum()} partial, "
    f"{(a == 255).sum()} opaque (of {a.size})"
)

# ── update index.html ─────────────────────────────────────────────
with open(INDEX_HTML) as f:
    html = f.read()

old_uris = re.findall(r"data:image/png;base64,[A-Za-z0-9+/=]+", html)
if not old_uris:
    die("no data URIs found in index.html")

new_uri = f"data:image/png;base64,{new_b64}"
for old in old_uris:
    html = html.replace(old, new_uri)

assert new_b64 in html, "new base64 not found after replacement!"

with open(INDEX_HTML, "w") as f:
    f.write(html)

print(f"Updated {INDEX_HTML} ({len(old_uris)} URI(s) replaced)")
print("Done!")
