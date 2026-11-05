# Encodes one second of a 440 Hz tone with libopus at 8 kbit/s, mono 48 kHz, VoIP, into
# tone_<ms>ms.bin: each packet as a little-endian u16 length and its bytes.
import ctypes, ctypes.util, math, struct, sys
from pathlib import Path

opus = ctypes.CDLL(ctypes.util.find_library("opus") or "libopus.so.0")
opus.opus_encoder_create.restype = ctypes.c_void_p
out_dir = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).parent)
for ms in (20, 40, 60, 120):
    err = ctypes.c_int()
    enc = ctypes.c_void_p(opus.opus_encoder_create(48000, 1, 2048, ctypes.byref(err)))
    assert err.value == 0, err.value
    assert opus.opus_encoder_ctl(enc, 4002, 8000) == 0  # OPUS_SET_BITRATE
    n = 48 * ms
    data = b""
    for f in range(48000 // n):
        pcm = (ctypes.c_float * n)(*(0.3 * math.sin(2 * math.pi * 440 * (f * n + i) / 48000) for i in range(n)))
        buf = (ctypes.c_ubyte * 1500)()
        size = opus.opus_encode_float(enc, pcm, n, buf, 1500)
        assert size > 0, size
        data += struct.pack("<H", size) + bytes(buf[:size])
    opus.opus_encoder_destroy(enc)
    (out_dir / f"tone_{ms}ms.bin").write_bytes(data)
