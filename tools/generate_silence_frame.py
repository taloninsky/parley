#!/usr/bin/env python3
"""Regenerate the canonical silent MP3 frame for SilenceSplicer.

Output path:
    proxy/src/tts/silence_frame_44100_128_stereo.mp3

Format:
    MPEG-1 Layer III, 44.1 kHz, 128 kbps, joint stereo, padded.
    Exactly 418 bytes covering 1152 PCM samples (~26.122 ms).

Why we re-extract from the second frame:
    LAME emits a leading "Info"/Xing tag frame (still silent, but
    with metadata in the side_info area). Repeating that frame in
    the splice stream confuses some decoders. The second frame is
    the first stable, repeating silent frame and is what we ship.

Usage (any Python with `pip install lameenc`):
    python tools/generate_silence_frame.py
"""

from __future__ import annotations

import sys
from pathlib import Path

try:
    import lameenc  # type: ignore[import-not-found]
except ModuleNotFoundError:
    sys.exit("missing dep: pip install lameenc")

OUT = Path(__file__).resolve().parent.parent \
    / "proxy" / "src" / "tts" / "silence_frame_44100_128_stereo.mp3"

enc = lameenc.Encoder()
enc.set_bit_rate(128)
enc.set_in_sample_rate(44100)
enc.set_channels(2)
enc.set_quality(2)

# Encode 10 frames of zero PCM so we're well past the Info tag.
pcm = bytes(1152 * 10 * 2 * 2)  # int16 stereo, all zeros
data = enc.encode(pcm) + enc.flush()

# Find sync words and grab frame index 1 (the first stable repeating
# silent frame; index 0 is the Info/Xing tag).
syncs = [i for i in range(len(data) - 1)
         if data[i] == 0xFF and (data[i + 1] & 0xE0) == 0xE0]
if len(syncs) < 3:
    sys.exit(f"expected ≥3 sync words, found {len(syncs)}")
frame = data[syncs[1]:syncs[2]]
if len(frame) != 418:
    sys.exit(f"expected 418-byte frame, got {len(frame)}")

OUT.write_bytes(frame)
print(f"wrote {len(frame)} bytes to {OUT}")
