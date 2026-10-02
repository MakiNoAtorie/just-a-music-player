"""Generate quiet, original WAV tones for local playback checks (stdlib only)."""
import math
import pathlib
import struct

output = pathlib.Path(__file__).resolve().parent.parent / "test-output"
output.mkdir(exist_ok=True)

def chunk(name, data):
    return name + struct.pack("<I", len(data)) + data + (b"\0" if len(data) % 2 else b"")

def wav(name, rate, seconds, frequency, title):
    frames = int(rate * seconds)
    samples = b"".join(struct.pack("<h", int(math.sin(i * frequency * math.tau / rate) * 1800)) for i in range(frames))
    fmt = struct.pack("<HHIIHH", 1, 1, rate, rate * 2, 2, 16)
    info = b"INFO" + chunk(b"INAM", title.encode() + b"\0") + chunk(b"IART", b"Maki sound check\0") + chunk(b"IPRD", b"A little space for music\0")
    body = b"WAVE" + chunk(b"fmt ", fmt) + chunk(b"LIST", info) + chunk(b"data", samples)
    (output / name).write_bytes(b"RIFF" + struct.pack("<I", len(body)) + body)

wav("01-first.wav", 44100, 1.3, 440, "First light")
wav("02-next.wav", 48000, 0.9, 660, "A small pause")
wav("preview.wav", 44100, 30, 220, "A little space for music")
wav("empty.wav", 44100, 0, 440, "Empty track")
(output / "broken.mp3").write_bytes(b"deliberately invalid audio")
print(output)
