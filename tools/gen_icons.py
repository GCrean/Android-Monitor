#!/usr/bin/env python3
"""Generate the application icon set into assets/ (needs Pillow).

  assets/icon.png      1024px master
  assets/icon.ico      Windows (16-256)
  assets/icon.icns     macOS
  assets/icons/NxN.png Linux hicolor sizes
"""
import math
import struct
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

OUT = Path(__file__).resolve().parent.parent / "assets"
S = 1024
SS = 2  # supersample


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def make_master():
    n = S * SS
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))

    # rounded-square background with a vertical gradient
    bg = Image.new("RGBA", (n, n))
    px = bg.load()
    top, bot = (22, 32, 48), (9, 13, 20)
    for y in range(n):
        c = lerp(top, bot, y / n)
        for x in range(n):
            px[x, y] = c + (255,)
    mask = Image.new("L", (n, n), 0)
    m = int(n * 0.04)
    ImageDraw.Draw(mask).rounded_rectangle((m, m, n - m, n - m), radius=int(n * 0.22), fill=255)
    img.paste(bg, (0, 0), mask)

    d = ImageDraw.Draw(img)
    green = (61, 220, 132, 255)
    dim = (61, 220, 132, 70)

    # grid lines
    for i in range(1, 4):
        y = int(n * (0.28 + i * 0.14))
        d.line((n * 0.17, y, n * 0.83, y), fill=(255, 255, 255, 22), width=int(n * 0.006))

    # telemetry line (smooth-ish polyline through control points)
    pts = [(0.17, 0.66), (0.30, 0.60), (0.40, 0.68), (0.50, 0.36), (0.60, 0.62), (0.70, 0.50), (0.83, 0.54)]
    path = []
    for (x0, y0), (x1, y1) in zip(pts, pts[1:]):
        for k in range(40):
            t = k / 40
            e = (1 - math.cos(t * math.pi)) / 2  # cosine easing -> smooth
            path.append((n * (x0 + (x1 - x0) * t), n * (y0 + (y1 - y0) * e)))
    path.append((n * pts[-1][0], n * pts[-1][1]))

    # soft area fill under the line
    area = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    ImageDraw.Draw(area).polygon(path + [(path[-1][0], n * 0.80), (path[0][0], n * 0.80)], fill=dim)
    img.alpha_composite(area)

    # glow + stroke
    glow = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    ImageDraw.Draw(glow).line(path, fill=(61, 220, 132, 200), width=int(n * 0.05), joint="curve")
    img.alpha_composite(glow.filter(ImageFilter.GaussianBlur(n * 0.02)))
    d = ImageDraw.Draw(img)
    d.line(path, fill=green, width=int(n * 0.036), joint="curve")
    for p in (path[0], path[-1]):
        r = n * 0.024
        d.ellipse((p[0] - r, p[1] - r, p[0] + r, p[1] + r), fill=green)

    # donut ring accent, top-left
    cx, cy, r = n * 0.30, n * 0.27, n * 0.075
    d.arc((cx - r, cy - r, cx + r, cy + r), 0, 360, fill=(255, 255, 255, 60), width=int(n * 0.022))
    d.arc((cx - r, cy - r, cx + r, cy + r), -90, 150, fill=(88, 166, 255, 255), width=int(n * 0.022))

    return img.resize((S, S), Image.LANCZOS)


def icns(master: Image.Image) -> bytes:
    import io

    chunks = b""
    for tag, size in ((b"icp4", 16), (b"icp5", 32), (b"icp6", 64), (b"ic07", 128), (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024)):
        buf = io.BytesIO()
        master.resize((size, size), Image.LANCZOS).save(buf, "PNG")
        data = buf.getvalue()
        chunks += tag + struct.pack(">I", len(data) + 8) + data
    return b"icns" + struct.pack(">I", len(chunks) + 8) + chunks


def main():
    OUT.mkdir(exist_ok=True)
    (OUT / "icons").mkdir(exist_ok=True)
    master = make_master()
    master.save(OUT / "icon.png")
    master.save(OUT / "icon.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    (OUT / "icon.icns").write_bytes(icns(master))
    for s in (16, 32, 48, 64, 128, 256, 512):
        master.resize((s, s), Image.LANCZOS).save(OUT / "icons" / f"{s}x{s}.png")
    print("wrote", OUT)


if __name__ == "__main__":
    main()
