#!/usr/bin/env python3
"""Render the wallpapers that ship with EDOS, as 24-bit BMP.

Generated rather than committed: this repo holds no binaries, and a picture
built from a formula is reproducible byte for byte, so `make` does not rebuild
the disk image on every run.

The output is deliberately not screen-sized. A wallpaper the compositor does
not have to scale never exercises the scaler, and the scaler is the part that
can be wrong.

    scripts/mkwallpaper.py filesystem/share/wallpapers
"""

import math
import struct
import sys
from pathlib import Path

WIDTH = 1600
HEIGHT = 1000


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


class Random:
    """A small LCG, so a wallpaper is identical on every machine.

    `random` seeds reproducibly too, but its stream is an implementation
    detail of the interpreter and this one is written down here.
    """

    def __init__(self, seed):
        self.state = seed & 0xFFFFFFFF

    def next(self):
        self.state = (self.state * 1664525 + 1013904223) & 0xFFFFFFFF
        return self.state

    def uniform(self, low, high):
        return low + (high - low) * (self.next() / 0xFFFFFFFF)


def ridge_height(x, width, base, seed, amplitude):
    """Height of one mountain ridge at `x`, as a sum of sine waves.

    Three octaves: one that gives the range its shape, one that gives it
    peaks, one that roughens the line so it does not read as a wave.
    """
    t = x / width
    rng = Random(seed)
    phases = [rng.uniform(0, math.tau) for _ in range(3)]
    value = (
        math.sin(t * math.tau * 1.0 + phases[0]) * 0.60
        + math.sin(t * math.tau * 2.7 + phases[1]) * 0.28
        + math.sin(t * math.tau * 6.3 + phases[2]) * 0.12
    )
    return base - value * amplitude


def render_dusk(width, height):
    """A dusk sky over four ridges, the far ones hazed by the air between."""
    sky_top = (13, 17, 34)
    sky_mid = (46, 38, 74)
    horizon = (196, 118, 88)

    sun_x, sun_y, sun_r = width * 0.68, height * 0.44, height * 0.055

    pixels = [[sky_top] * width for _ in range(height)]

    for y in range(height):
        t = y / (height - 1)
        # Two stops rather than one: a single ramp from night to a warm
        # horizon passes through colours that are in neither.
        if t < 0.62:
            row = lerp(sky_top, sky_mid, t / 0.62)
        else:
            row = lerp(sky_mid, horizon, ((t - 0.62) / 0.38) ** 2.2)
        for x in range(width):
            pixels[y][x] = row

    # Stars, thinning out as the sky brightens toward the horizon.
    rng = Random(0x5EED_1234)
    for _ in range(1400):
        x = int(rng.uniform(0, width))
        y = int(rng.uniform(0, height * 0.60))
        fade = 1.0 - (y / (height * 0.60)) ** 0.7
        brightness = rng.uniform(0.15, 1.0) * fade
        if brightness <= 0.05:
            continue
        pixels[y][x] = lerp(pixels[y][x], (255, 250, 235), brightness)

    # The sun, and the glow it throws into the sky around it.
    for y in range(height):
        for x in range(width):
            d = math.hypot(x - sun_x, y - sun_y)
            if d < sun_r:
                pixels[y][x] = (255, 236, 200)
            elif d < sun_r * 9:
                glow = (1.0 - (d - sun_r) / (sun_r * 8)) ** 3
                pixels[y][x] = lerp(pixels[y][x], (255, 190, 130), glow * 0.55)

    # Ridges back to front: each is darker and less hazed than the one behind.
    ridges = [
        (0.60, 0x1111, height * 0.075, (74, 66, 96), 0.55),
        (0.68, 0x2222, height * 0.090, (54, 48, 78), 0.35),
        (0.78, 0x3333, height * 0.105, (34, 30, 56), 0.18),
        (0.90, 0x4444, height * 0.120, (16, 14, 30), 0.00),
    ]
    for base_frac, seed, amplitude, colour, haze in ridges:
        base = height * base_frac
        hazed = lerp(colour, horizon, haze)
        for x in range(width):
            top = int(ridge_height(x, width, base, seed, amplitude))
            for y in range(max(top, 0), height):
                # The near face picks up a little of the sky it faces, so the
                # ridge line reads as an edge rather than a cut-out.
                depth = min((y - top) / (height * 0.12), 1.0)
                pixels[y][x] = lerp(lerp(hazed, sky_mid, 0.25 * (1 - depth)), hazed, depth)

    return pixels


def write_bmp(path, pixels):
    """24-bit BI_RGB, rows bottom-up, each padded to four bytes."""
    height = len(pixels)
    width = len(pixels[0])
    padding = b"\0" * (-(width * 3) % 4)
    rows = bytearray()
    for row in reversed(pixels):
        for r, g, b in row:
            rows += bytes((b & 0xFF, g & 0xFF, r & 0xFF))
        rows += padding

    start = 14 + 40
    header = b"BM" + struct.pack("<IHHI", start + len(rows), 0, 0, start)
    dib = struct.pack("<IiiHHIIiiII", 40, width, height, 1, 24, 0, len(rows), 2835, 2835, 0, 0)
    path.write_bytes(header + dib + bytes(rows))


def main():
    out_dir = Path(sys.argv[1] if len(sys.argv) > 1 else "filesystem/share/wallpapers")
    out_dir.mkdir(parents=True, exist_ok=True)
    write_bmp(out_dir / "dusk.bmp", render_dusk(WIDTH, HEIGHT))


if __name__ == "__main__":
    main()
