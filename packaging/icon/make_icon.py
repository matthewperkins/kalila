#!/usr/bin/env python3
"""Build the oers icon: Bastet and Sekhmet face to face, with two traces.

    python3 packaging/icon/make_icon.py        # writes oers-icon.svg and PNGs next to this file

Bastet (left, mirrored to face right) gives off a slow, gastric-like wave from her pupil, in
her iris blue, running out to the right over Sekhmet's collar. Sekhmet (right) receives a
single sharp action potential, in her iris red-orange, entering from the left edge and
ending in her pupil: slow against fast.

The drawings are placed by reference from src/ (unchanged originals; see ICON-CREDITS.md).
PNGs are rendered with rsvg-convert. All geometry is in 512-px tile units.
"""
import math
import os
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
TILE = 512
FACE = 250  # displayed width of each face window (height follows the window's aspect)
BG = "#1b2430"
GOLD = "#fac53a"
BASTET_IRIS = "#8fb4ea"
SEKHMET_IRIS = "#c13c1b"

# Face windows in each drawing's own units (x, y, width, height) and pupil centres, measured
# from the renders (iris pixels, then the black pupil inside them).
B_WIN = (155.0, 108.0, 107.0, 190.0)
B_PUPIL = (186.73, 153.67)
S_WIN = (154.0, 122.0, 107.0, 190.0)
S_PUPIL = (187.11, 159.89)
B_DEST = (6, 104)  # tile position of Bastet's window (pushed down relative to Sekhmet)
S_DEST = (256, 72)
SPIKE_X = 196.0  # where Sekhmet's spike fires (tile x)


def b_to_tile(x, y):  # Bastet is mirrored within her window
    k = FACE / B_WIN[2]
    return B_DEST[0] + (B_WIN[0] + B_WIN[2] - x) * k, B_DEST[1] + (y - B_WIN[1]) * k


def s_to_tile(x, y):
    k = FACE / S_WIN[2]
    return S_DEST[0] + (x - S_WIN[0]) * k, S_DEST[1] + (y - S_WIN[1]) * k


def path(points):
    return "M" + " L".join(f"{x:.2f},{y:.2f}" for x, y in points)


def slow_wave(x0, y0, x1):
    """Gastric-like slow wave leaving Bastet's pupil (x0, y0) rightwards (two harmonics)."""
    pts = []
    period, amp = 290.0, 120.0
    n = int((x1 - x0) / 1.5)
    for i in range(n + 1):
        x = x0 + (x1 - x0) * i / n
        ph = 2 * math.pi * (x - x0) / period
        # Up a little first (clearing Bastet's snout), then down over Sekhmet's collar and up
        # again: an asymmetric two-harmonic wave.
        v = (1 - math.cos(ph)) / 2 - 0.42 * math.sin(ph) + 0.12 * math.sin(2 * ph)
        pts.append((x, y0 + amp * v))
    return pts


def spike_trace(x0, x1, y0):
    """Flat baseline from the left edge (x0) to the pupil (x1, y0), with one sharp action
    potential: fast rise, slightly slower fall, a small after-hyperpolarisation. Up = depolarised."""
    ts, amp = SPIKE_X, 118.0
    rise, fall = 5.0, 9.0

    def v(x):
        d = x - ts
        if -rise <= d < 0:
            return amp * (1 - (d / rise) ** 2)  # rounded tip, steep flanks
        if 0 <= d < fall:
            return amp * (1 - d / fall) ** 1.6
        if fall <= d < fall + 34:  # AHP, recovering
            u = (d - fall) / 34
            return -16 * math.sin(math.pi * u) * (1 - u) ** 0.5
        return 0.0

    xs = sorted({x0 + i * 0.5 for i in range(int((x1 - x0) / 0.5) + 1)} | {ts - rise, ts, ts + fall})
    return [(x, y0 - v(x)) for x in xs if x0 <= x <= x1]


def build():
    bx, by = b_to_tile(*B_PUPIL)
    sx, sy = s_to_tile(*S_PUPIL)
    k_b = FACE / B_WIN[2]
    k_s = FACE / S_WIN[2]
    wave = path(slow_wave(bx, by, TILE - 4))
    burst = path(spike_trace(4, sx, sy))
    under = f'fill="none" stroke="{BG}" stroke-width="17" stroke-linecap="round" stroke-linejoin="round"'
    return f'''<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="{TILE}" height="{TILE}" viewBox="0 0 {TILE} {TILE}">
  <title>oers</title>
  <!-- Adapted from Bastet by Gunawan Kartapranata (CC BY-SA 3.0) and Sekhmet by Jeff Dahl
       (CC BY-SA 4.0), Wikimedia Commons; see ICON-CREDITS.md. This icon: CC BY-SA 4.0. -->
  <defs>
    <clipPath id="tile"><rect x="8" y="8" width="496" height="496" rx="80"/></clipPath>
    <linearGradient id="fade" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{BG}" stop-opacity="0"/><stop offset="1" stop-color="{BG}" stop-opacity="0.92"/>
    </linearGradient>
  </defs>
  <g clip-path="url(#tile)">
    <rect width="{TILE}" height="{TILE}" fill="{BG}"/>
    <g transform="translate({B_DEST[0] + FACE} {B_DEST[1]}) scale(-1 1)">
      <svg width="{FACE}" height="{B_WIN[3] * k_b:.2f}" viewBox="{B_WIN[0]} {B_WIN[1]} {B_WIN[2]} {B_WIN[3]}">
        <image xlink:href="src/Bastet.svg" width="350" height="757"/>
      </svg>
    </g>
    <svg x="{S_DEST[0]}" y="{S_DEST[1]}" width="{FACE}" height="{S_WIN[3] * k_s:.2f}" viewBox="{S_WIN[0]} {S_WIN[1]} {S_WIN[2]} {S_WIN[3]}">
      <image xlink:href="src/Sekhmet.svg" width="350" height="765"/>
    </svg>
    <!-- Fade the busy lower bodies towards the background, keeping the eye on the faces. -->
    <rect x="0" y="392" width="{TILE}" height="{TILE - 392}" fill="url(#fade)"/>
    <path d="{burst}" fill="none" stroke="{BG}" stroke-width="15" stroke-linecap="round" stroke-linejoin="round"/>
    <path d="{burst}" fill="none" stroke="{SEKHMET_IRIS}" stroke-width="8" stroke-linecap="round" stroke-linejoin="round"/>
    <path d="{wave}" {under}/>
    <path d="{wave}" fill="none" stroke="{BASTET_IRIS}" stroke-width="9" stroke-linecap="round" stroke-linejoin="round"/>
    <!-- The traces start/end in the pupils: redraw each iris and pupil on top. -->
    <circle cx="{bx:.2f}" cy="{by:.2f}" r="{3.03 * k_b:.2f}" fill="{BASTET_IRIS}" stroke="#000" stroke-width="1.5"/>
    <circle cx="{bx:.2f}" cy="{by:.2f}" r="{1.3 * k_b:.2f}" fill="#000"/>
    <circle cx="{sx:.2f}" cy="{sy:.2f}" r="{2.81 * k_s:.2f}" fill="{SEKHMET_IRIS}" stroke="#000" stroke-width="1.5"/>
    <circle cx="{sx:.2f}" cy="{sy:.2f}" r="{1.2 * k_s:.2f}" fill="#000"/>
  </g>
  <rect x="13" y="13" width="486" height="486" rx="76" fill="none" stroke="{GOLD}" stroke-width="10"/>
</svg>
'''


def main():
    svg = os.path.join(HERE, "oers-icon.svg")
    with open(svg, "w") as f:
        f.write(build())
    for size in (256, 128, 64, 48, 32):
        out = os.path.join(HERE, f"oers-{size}.png")
        subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size), svg, "-o", out], check=True, cwd=HERE)
    print(f"wrote {svg} and oers-{{256,128,64,48,32}}.png")


if __name__ == "__main__":
    main()
