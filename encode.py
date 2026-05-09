#!/usr/bin/env python3

import sys
import os
import json
import hashlib
import math
import struct
import subprocess
import tempfile
import wave
import numpy as np
from pathlib import Path

import reedsolo
import qrcode

# Video
FRAME_WIDTH = 1920
FRAME_HEIGHT = 1080
FRAME_RATE = 30
BLOCK_SIZE = 64
COLS = FRAME_WIDTH // BLOCK_SIZE    # 30
ROWS = FRAME_HEIGHT // BLOCK_SIZE   # 16
TOTAL_BLOCKS = COLS * ROWS          # 480

# Frame layout: [8 sync bits][frame index bits][data bits]
SYNC_PATTERN = np.array([1, 0, 1, 0, 1, 1, 0, 0], dtype=np.uint8)
SYNC_BITS = len(SYNC_PATTERN)
DEFAULT_FRAME_INDEX_BITS = 16
EXTENDED_FRAME_INDEX_BITS = 32
MAX_FRAME_INDEX = (1 << EXTENDED_FRAME_INDEX_BITS) - 1
MAX_FRAME_COUNT_DEFAULT = 1 << DEFAULT_FRAME_INDEX_BITS
MAX_FRAME_COUNT_EXTENDED = MAX_FRAME_INDEX + 1

# Audio (4-FSK)
SAMPLE_RATE = 44100
BAUD_RATE = 100                     # symbols/sec
BITS_PER_SYMBOL = 2
SAMPLES_PER_SYMBOL = SAMPLE_RATE // BAUD_RATE  # 441 samples/symbol
FSK_FREQS = [1000, 1200, 1400, 1600]           # Hz for symbols 0-3
BYTES_PER_SEC_AUDIO = (BAUD_RATE * BITS_PER_SYMBOL) // 8  # 25 bytes/sec

# Reed-Solomon
RS_ECC_SYMBOLS = 32                 # ECC bytes per 255-byte RS block

# Metadata QR
METADATA_DURATION_SEC = 1.0
METADATA_FRAMES = max(1, int(round(FRAME_RATE * METADATA_DURATION_SEC)))
QR_BORDER_MODULES = 4
QR_ERROR_CORRECTION = qrcode.constants.ERROR_CORRECT_Q
QR_MIN_MODULE_PX = 6

ENCODING_VERSION = 3


def build_payload(filepath: str):
    path = Path(filepath)
    raw = path.read_bytes()
    sha256 = hashlib.sha256(raw).hexdigest()
    meta = {
        "v": ENCODING_VERSION,
        "filename": path.name,
        "size": len(raw),
        "sha256": sha256,
    }
    meta_bytes = json.dumps(meta, separators=(",", ":")).encode()
    # Layout: [4 bytes: meta_len][meta JSON][raw file bytes]
    payload = struct.pack(">I", len(meta_bytes)) + meta_bytes + raw
    return payload, meta


def build_qr_metadata(meta: dict, ecc_len: int, num_frames: int, index_bits: int) -> bytes:
    qr_meta = {
        "v": ENCODING_VERSION,
        "f": meta["filename"],
        "s": meta["size"],
        "h": meta["sha256"],
        "e": ecc_len,
        "n": num_frames,
        "i": index_bits,
        "m": METADATA_FRAMES,
    }
    return json.dumps(qr_meta, separators=(",", ":")).encode()


def ecc_encode(data: bytes) -> bytes:
    # reedsolo chunks into (255 - RS_ECC_SYMBOLS) = 223 byte data blocks
    # Each block gets RS_ECC_SYMBOLS parity bytes appended
    print(f"  Input: {len(data):,} bytes")
    rs = reedsolo.RSCodec(RS_ECC_SYMBOLS)
    encoded = bytes(rs.encode(data))
    print(f"  ECC output: {len(encoded):,} bytes ({(len(encoded)/len(data)-1)*100:.1f}% overhead)")
    return encoded


def frame_layout(index_bits: int) -> tuple[int, int]:
    header_bits = SYNC_BITS + index_bits
    data_bits_per_frame = TOTAL_BLOCKS - header_bits
    if data_bits_per_frame <= 0:
        raise ValueError(
            f"Frame index bits ({index_bits}) exceed available block capacity ({TOTAL_BLOCKS} blocks)."
        )
    if data_bits_per_frame % 8 != 0:
        raise ValueError(
            f"Frame data bits ({data_bits_per_frame}) must align to full bytes (multiples of 8)."
        )
    return header_bits, data_bits_per_frame


def index_to_bits(idx: int, index_bits: int) -> np.ndarray:
    return np.array(
        [(idx >> (index_bits - 1 - i)) & 1 for i in range(index_bits)],
        dtype=np.uint8
    )


def make_frame(frame_idx: int, data_bits: np.ndarray, index_bits: int) -> bytes:
    header = np.concatenate([SYNC_PATTERN, index_to_bits(frame_idx, index_bits)])
    block_bits = np.concatenate([header, data_bits])            # (480,)
    grid = (block_bits.reshape(ROWS, COLS) * 255).astype(np.uint8)  # (16, 30)
    frame_2d = np.repeat(np.repeat(grid, BLOCK_SIZE, axis=0), BLOCK_SIZE, axis=1)  # (1080, 1920)
    # Pad the unused bottom 56 pixels so every frame is a true 1920x1080 buffer.
    full_frame = np.zeros((FRAME_HEIGHT, FRAME_WIDTH), dtype=np.uint8)
    full_frame[:frame_2d.shape[0], :frame_2d.shape[1]] = frame_2d
    frame_rgb = np.stack([full_frame, full_frame, full_frame], axis=2)    # (1080, 1920, 3)
    return frame_rgb.tobytes()


def make_qr_frame(payload: bytes) -> bytes:
    qr = qrcode.QRCode(
        error_correction=QR_ERROR_CORRECTION,
        box_size=1,
        border=QR_BORDER_MODULES,
    )
    qr.add_data(payload)
    qr.make(fit=True)
    matrix = np.array(qr.get_matrix(), dtype=np.uint8)
    modules = matrix.shape[0]

    max_width = FRAME_WIDTH - 2 * BLOCK_SIZE
    max_height = FRAME_HEIGHT - 2 * BLOCK_SIZE
    module_px = min(max_width // modules, max_height // modules)
    if module_px < QR_MIN_MODULE_PX:
        raise ValueError(
            f"QR modules too small ({module_px}px). "
            f"Shorten metadata or reduce QR error correction."
        )

    qr_pixels = np.where(matrix == 1, 0, 255).astype(np.uint8)
    qr_pixels = np.kron(qr_pixels, np.ones((module_px, module_px), dtype=np.uint8))

    qr_frame = np.full((FRAME_HEIGHT, FRAME_WIDTH), 255, dtype=np.uint8)
    h, w = qr_pixels.shape
    y0 = (FRAME_HEIGHT - h) // 2
    x0 = (FRAME_WIDTH - w) // 2
    qr_frame[y0:y0 + h, x0:x0 + w] = qr_pixels

    frame_rgb = np.stack([qr_frame, qr_frame, qr_frame], axis=2)
    return frame_rgb.tobytes()


def make_audio_pcm(data: bytes, duration_sec: float) -> bytes:
    bits = np.unpackbits(np.frombuffer(data, dtype=np.uint8))

    # Group into 2-bit symbols
    n = (len(bits) // BITS_PER_SYMBOL) * BITS_PER_SYMBOL
    symbols = bits[:n].reshape(-1, BITS_PER_SYMBOL)
    symbols = symbols[:, 0] * 2 + symbols[:, 1]    # (N,) values 0-3

    total_samples = int(SAMPLE_RATE * duration_sec)
    max_symbols = total_samples // SAMPLES_PER_SYMBOL
    symbols = symbols[:max_symbols]

    # Vectorized tone generation: (N, SAMPLES_PER_SYMBOL)
    freqs = np.array(FSK_FREQS)[symbols].astype(np.float32)     # (N,)
    t = np.arange(SAMPLES_PER_SYMBOL, dtype=np.float32) / SAMPLE_RATE  # (S,)
    phases = 2 * np.pi * freqs[:, np.newaxis] * t[np.newaxis, :]
    tones = np.sin(phases).flatten().astype(np.float32)

    audio = np.zeros(total_samples, dtype=np.float32)
    audio[:len(tones)] = tones

    peak = np.max(np.abs(audio))
    if peak > 0:
        audio = audio / peak * 0.9

    return (audio * 32767).astype(np.int16).tobytes()


def write_wav(pcm: bytes, path: str):
    with wave.open(path, "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(SAMPLE_RATE)
        wf.writeframes(pcm)


def encode(input_path: str, output_path: str):
    print(f"[1/5] Reading file: {input_path}")
    payload, meta = build_payload(input_path)
    print(f"      {meta['filename']} | {meta['size']:,} bytes")
    print(f"      SHA256: {meta['sha256']}")
    print(f"      Payload (metadata + data): {len(payload):,} bytes")

    print("\n[2/5] Reed-Solomon ECC...")
    ecc_data = ecc_encode(payload)

    total_bits = len(ecc_data) * 8
    _, data_bits_default = frame_layout(DEFAULT_FRAME_INDEX_BITS)
    frames_16 = math.ceil(total_bits / data_bits_default)
    if frames_16 <= MAX_FRAME_COUNT_DEFAULT:
        index_bits = DEFAULT_FRAME_INDEX_BITS
        data_bits_per_frame = data_bits_default
        num_frames = frames_16
    else:
        _, data_bits_per_frame = frame_layout(EXTENDED_FRAME_INDEX_BITS)
        num_frames = math.ceil(total_bits / data_bits_per_frame)
        if num_frames > MAX_FRAME_COUNT_EXTENDED:
            raise ValueError(
                f"Payload needs {num_frames} frames which exceeds 32-bit frame index capacity "
                f"(max frame count {MAX_FRAME_COUNT_EXTENDED})."
            )
        index_bits = EXTENDED_FRAME_INDEX_BITS

    all_bits = np.unpackbits(np.frombuffer(ecc_data, dtype=np.uint8))
    rem = len(all_bits) % data_bits_per_frame
    if rem:
        all_bits = np.concatenate([all_bits, np.zeros(data_bits_per_frame - rem, dtype=np.uint8)])

    num_frames = len(all_bits) // data_bits_per_frame
    qr_payload = build_qr_metadata(meta, len(ecc_data), num_frames, index_bits)
    qr_frame = make_qr_frame(qr_payload)
    total_frames = num_frames + METADATA_FRAMES
    duration_sec = total_frames / FRAME_RATE

    video_bps = (data_bits_per_frame * FRAME_RATE) // 8
    audio_byte_count = min(len(ecc_data), int(duration_sec * BYTES_PER_SEC_AUDIO))
    audio_coverage_pct = audio_byte_count / len(ecc_data) * 100

    print(f"\n[3/5] Plan")
    print(f"      Metadata frames:{METADATA_FRAMES:>10} ({METADATA_DURATION_SEC:.1f}s)")
    print(f"      Data frames:    {num_frames:>10}")
    print(f"      Total frames:   {total_frames:>10} @ {FRAME_RATE}fps ({duration_sec:.1f}s)")
    print(f"      Frame index:    {index_bits} bits")
    print(f"      Video channel:  {video_bps:,} bytes/sec ({data_bits_per_frame} bits/frame)")
    print(f"      Audio channel:  {BYTES_PER_SEC_AUDIO} bytes/sec (4-FSK @ {BAUD_RATE} baud)")
    print(f"      Audio covers:   {audio_byte_count:,}/{len(ecc_data):,} bytes ({audio_coverage_pct:.1f}%)")
    if audio_coverage_pct < 100:
        print(f"      Note: audio carries only the first {audio_coverage_pct:.1f}% of data;")
        print(f"            increase BAUD_RATE or use a shorter file for full coverage.")

    with tempfile.TemporaryDirectory() as tmp:
        audio_path = os.path.join(tmp, "audio.wav")

        print(f"\n[4/5] Generating audio track...")
        pcm = make_audio_pcm(ecc_data[:audio_byte_count], duration_sec)
        write_wav(pcm, audio_path)
        print(f"      Written: {os.path.getsize(audio_path):,} bytes (WAV, mono, {SAMPLE_RATE}Hz)")

        print(f"\n[5/5] Encoding video frames and muxing...")
        ffmpeg_cmd = [
            "ffmpeg", "-y",
            "-f", "rawvideo",
            "-pixel_format", "rgb24",
            "-video_size", f"{FRAME_WIDTH}x{FRAME_HEIGHT}",
            "-framerate", str(FRAME_RATE),
            "-i", "pipe:0",
            "-i", audio_path,
            "-c:v", "libx264",
            "-crf", "0",           # lossless H.264 for the local source file
            "-preset", "ultrafast",
            "-pix_fmt", "yuv420p", # required for YouTube compatibility
            "-c:a", "aac",
            "-b:a", "320k",
            output_path,
        ]

        proc = subprocess.Popen(
            ffmpeg_cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        for _ in range(METADATA_FRAMES):
            proc.stdin.write(qr_frame)

        written = METADATA_FRAMES
        for i in range(num_frames):
            chunk = all_bits[i * data_bits_per_frame:(i + 1) * data_bits_per_frame]
            proc.stdin.write(make_frame(i, chunk, index_bits))
            written += 1
            if written % 30 == 0 or written == total_frames:
                pct = written / total_frames * 100
                bar = "#" * (int(pct) // 2) + "-" * (50 - int(pct) // 2)
                print(f"      [{bar}] {pct:5.1f}%  ({written}/{total_frames})", end="\r", flush=True)

        proc.stdin.close()
        ret = proc.wait()
        print()

        if ret != 0:
            sys.exit("ffmpeg exited with an error. Make sure ffmpeg is installed and libx264 is available.")

    size_mb = os.path.getsize(output_path) / 1_000_000
    print(f"\nDone.")
    print(f"  Output:   {output_path} ({size_mb:.1f} MB)")
    print(f"  Duration: {duration_sec:.1f}s")
    print(f"  Frames:   {num_frames:,}")
    print(f"  Upload this file to YouTube at 1080p60 or higher.")
    print(f"  Decode by downloading at 1080p and extracting frames with ffmpeg.")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(f"Usage: python {sys.argv[0]} <input_file> <output.mp4>")
        print(f"  <input_file>  any file you want to encode (binary or text)")
        print(f"  <output.mp4>  YouTube-ready video output")
        sys.exit(1)

    encode(sys.argv[1], sys.argv[2])
