#!/usr/bin/env python3
"""Render the sounds that ship with EDOS, as 16-bit stereo PCM WAV.

Generated rather than committed, for the same reason as the wallpapers: this
repo holds no binaries, and a waveform built from a formula is reproducible
byte for byte, so `make` does not rebuild the disk image on every run.

    scripts/mksounds.py filesystem/share/sounds
"""

import math
import struct
import sys
import wave
from pathlib import Path

SAMPLE_RATE = 44100
CHANNELS = 2
AMPLITUDE = 12000

# A two-note chime: a fifth, each note decaying into the next.
NOTES = [(880.0, 0.22), (1318.5, 0.38)]


def chime():
    frames = bytearray()
    for freq, seconds in NOTES:
        count = int(SAMPLE_RATE * seconds)
        for i in range(count):
            t = i / SAMPLE_RATE
            # Exponential decay, so the note ends at silence and the join
            # between the two carries no click.
            envelope = math.exp(-5.0 * i / count)
            sample = int(AMPLITUDE * envelope * math.sin(2 * math.pi * freq * t))
            frames += struct.pack("<hh", sample, sample)
    return bytes(frames)


def main():
    out_dir = Path(sys.argv[1] if len(sys.argv) > 1 else "filesystem/share/sounds")
    out_dir.mkdir(parents=True, exist_ok=True)
    with wave.open(str(out_dir / "chime.wav"), "wb") as w:
        w.setnchannels(CHANNELS)
        w.setsampwidth(2)
        w.setframerate(SAMPLE_RATE)
        w.writeframes(chime())


if __name__ == "__main__":
    main()
