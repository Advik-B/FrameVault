#!/usr/bin/env python3

import sys
import os
import json
import hashlib
import struct
import subprocess
import numpy as np
from pathlib import Path

try:
    import reedsolo
except ImportError:
    sys.exit("reedsolo not found. Run: pip install reedsolo")

# Must match encoder exactly
FRAME_WIDTH = 1920
FRAME_HEIGHT = 1080
FRAME_RATE = 30
BLOCK_SIZE = 64
COLS = FRAME_WIDTH // BLOCK_SIZE    # 30
ROWS = FRAME_HEIGHT // BLOCK_SIZE   # 16
TOTAL_BLOCKS = COLS * ROWS          # 480

SYNC_PATTERN = np.array([1, 0, 1, 0, 1, 1, 0, 0], dtype=np.uint8)
FRAME_INDEX_BITS = 16
HEADER_BITS = len(SYNC_PATTERN) + FRAME_INDEX_BITS  # 24
DATA_BITS_PER_FRAME = TOTAL_BLOCKS - HEADER_BITS    # 456
DATA_BYTES_PER_FRAME = DATA_BITS_PER_FRAME // 8     # 57

SAMPLE_RATE = 44100
BAUD_RATE = 100
BITS_PER_SYMBOL = 2
SAMPLES_PER_SYMBOL = SAMPLE_RATE // BAUD_RATE       # 441
FSK_FREQS = [1000, 1200, 1400, 1600]

RS_ECC_SYMBOLS = 32

# How many pixels in from each block edge to sample.
# Block edges are where DCT compression artifacts cluster.
# Center region (64 - 2*16 = 32x32 px) is much cleaner.
BLOCK_SAMPLE_MARGIN = 16
THRESHOLD = 128


def stream_frames(video_path: str):
    # Pipe raw RGB frames from ffmpeg one at a time to avoid loading the full video into RAM.
    # Map only the video stream and let ffmpeg output every decoded frame in source order.
    cmd = [
        "ffmpeg", "-i", video_path,
        "-map", "0:v:0",
        "-vf", f"scale={FRAME_WIDTH}:{FRAME_HEIGHT}",
        "-f", "rawvideo",
        "-pix_fmt", "rgb24",
        "pipe:1",
    ]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    frame_size = FRAME_WIDTH * FRAME_HEIGHT * 3
    while True:
        raw = proc.stdout.read(frame_size)
        if len(raw) < frame_size:
            break
        yield np.frombuffer(raw, dtype=np.uint8).reshape(FRAME_HEIGHT, FRAME_WIDTH, 3)
    proc.wait()


def read_blocks(frame: np.ndarray) -> np.ndarray:
    # Sample the center region of each 64x64 block to avoid edge compression artifacts.
    # Returns a flat (480,) binary array.
    m = BLOCK_SAMPLE_MARGIN
    luma = frame[:, :, 0]  # R channel is sufficient for grayscale frames
    bits = np.empty(TOTAL_BLOCKS, dtype=np.uint8)
    for i in range(TOTAL_BLOCKS):
        r = i // COLS
        c = i % COLS
        y0 = r * BLOCK_SIZE + m
        y1 = (r + 1) * BLOCK_SIZE - m
        x0 = c * BLOCK_SIZE + m
        x1 = (c + 1) * BLOCK_SIZE - m
        bits[i] = 1 if np.mean(luma[y0:y1, x0:x1]) >= THRESHOLD else 0
    return bits


def decode_frame(bits: np.ndarray):
    # Returns (frame_idx, data_bits) or (None, None) if sync check fails.
    sync = bits[:len(SYNC_PATTERN)]
    if not np.array_equal(sync, SYNC_PATTERN):
        return None, None
    idx_bits = bits[len(SYNC_PATTERN):HEADER_BITS]
    frame_idx = int(np.packbits(np.pad(idx_bits, (16 - FRAME_INDEX_BITS, 0)))[1])
    # Cleaner index decode:
    frame_idx = 0
    for b in idx_bits:
        frame_idx = (frame_idx << 1) | int(b)
    data_bits = bits[HEADER_BITS:]
    return frame_idx, data_bits


def extract_audio(video_path: str) -> np.ndarray:
    cmd = [
        "ffmpeg", "-i", video_path,
        "-f", "s16le",
        "-ac", "1",
        "-ar", str(SAMPLE_RATE),
        "pipe:1",
    ]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    raw = proc.stdout.read()
    proc.wait()
    samples = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0
    return samples


def decode_fsk(samples: np.ndarray) -> bytes:
    n_symbols = len(samples) // SAMPLES_PER_SYMBOL
    if n_symbols == 0:
        return b""

    # Batch FFT across all symbol windows at once
    windows = samples[:n_symbols * SAMPLES_PER_SYMBOL].reshape(n_symbols, SAMPLES_PER_SYMBOL)
    fft_mags = np.abs(np.fft.rfft(windows, axis=1))  # (n_symbols, freq_bins)

    freq_axis = np.fft.rfftfreq(SAMPLES_PER_SYMBOL, 1.0 / SAMPLE_RATE)
    target_bins = np.array([np.argmin(np.abs(freq_axis - f)) for f in FSK_FREQS])

    target_mags = fft_mags[:, target_bins]              # (n_symbols, 4)
    symbols = np.argmax(target_mags, axis=1)            # (n_symbols,) values 0-3

    bits = np.empty(n_symbols * BITS_PER_SYMBOL, dtype=np.uint8)
    bits[0::2] = (symbols >> 1) & 1
    bits[1::2] = symbols & 1

    n_bytes = len(bits) // 8
    return np.packbits(bits[:n_bytes * 8]).tobytes()


def rs_decode(data: bytes) -> bytes:
    rs = reedsolo.RSCodec(RS_ECC_SYMBOLS)
    decoded, _, _ = rs.decode(data)
    return bytes(decoded)


def parse_payload(payload: bytes):
    meta_len = struct.unpack(">I", payload[:4])[0]
    meta = json.loads(payload[4:4 + meta_len])
    file_start = 4 + meta_len
    file_data = payload[file_start:file_start + meta["size"]]
    return meta, file_data


def bits_to_bytes(bits: np.ndarray) -> bytes:
    n = (len(bits) // 8) * 8
    return np.packbits(bits[:n]).tobytes()


def decode(video_path: str, output_dir: str = "."):
    print(f"Input: {video_path}")

    # Step 1: video channel
    print("\n[1/5] Decoding video channel...")
    frames = {}
    total = 0
    sync_fail = 0

    for frame_np in stream_frames(video_path):
        total += 1
        bits = read_blocks(frame_np)
        idx, data_bits = decode_frame(bits)
        if idx is None:
            sync_fail += 1
        else:
            frames[idx] = data_bits
        if total % 30 == 0:
            print(f"  {total} frames read, {len(frames)} valid, {sync_fail} sync failures", end="\r", flush=True)

    print()
    print(f"  Total: {total} | Valid: {len(frames)} | Sync failures: {sync_fail}")

    if not frames:
        sys.exit("No valid frames found. Check that the video is 1080p and was encoded with yt_encode.py.")

    # Step 2: reassemble video bits
    print(f"\n[2/5] Reassembling frame data...")
    max_idx = max(frames.keys())
    total_bits = (max_idx + 1) * DATA_BITS_PER_FRAME
    all_bits = np.zeros(total_bits, dtype=np.uint8)
    missing = []

    for i in range(max_idx + 1):
        if i in frames:
            all_bits[i * DATA_BITS_PER_FRAME:(i + 1) * DATA_BITS_PER_FRAME] = frames[i]
        else:
            missing.append(i)

    if missing:
        print(f"  Missing {len(missing)} frames: {missing[:10]}{'...' if len(missing)>10 else ''}")
        print(f"  Reed-Solomon will attempt recovery ({len(missing)*DATA_BYTES_PER_FRAME} bytes zeroed).")
    else:
        print(f"  All {max_idx+1} frames present.")

    ecc_from_video = bits_to_bytes(all_bits)
    print(f"  Video ECC stream: {len(ecc_from_video):,} bytes")

    # Step 3: audio channel
    print(f"\n[3/5] Decoding audio channel (4-FSK @ {BAUD_RATE} baud)...")
    audio_samples = extract_audio(video_path)
    audio_bytes = decode_fsk(audio_samples)
    coverage_pct = len(audio_bytes) / len(ecc_from_video) * 100 if ecc_from_video else 0
    print(f"  Recovered: {len(audio_bytes):,} bytes ({coverage_pct:.1f}% of ECC stream)")

    # Step 4: RS decode — try strategies in order of confidence
    print(f"\n[4/5] Reed-Solomon decode...")
    payload = None

    # Each RS block is exactly 255 bytes. The video stream may have up to 56 bytes of
    # trailing zero-padding from frame alignment. Truncating to the nearest 255-byte
    # boundary strips that garbage without touching any real ECC data.
    rs_block_size = 255
    ecc_video = ecc_from_video[: (len(ecc_from_video) // rs_block_size) * rs_block_size]

    # Strategy A: video only — best for local lossless files where video is perfect
    # but audio (AAC→FSK) is noisy. Trying this first avoids overwriting correct
    # video bytes with noisy audio bytes.
    print(f"  Trying video-only ({len(ecc_video):,} bytes)...")
    try:
        payload = rs_decode(ecc_video)
        print(f"  RS decode succeeded on video stream.")
    except reedsolo.ReedSolomonError as e:
        print(f"  Video-only RS failed: {e}")

    # Strategy B: merge audio into video for bytes audio covers, then RS decode.
    # Rationale: audio and video degrade independently (different codecs, different
    # error patterns). Bytes that video mangled may be intact in audio and vice versa.
    # RS then corrects whatever residual errors remain.
    if payload is None and len(audio_bytes) > 0:
        merged = bytearray(ecc_video)
        for i in range(min(len(audio_bytes), len(merged))):
            merged[i] = audio_bytes[i]
        print(f"  Trying merged (audio+video)...")
        try:
            payload = rs_decode(bytes(merged))
            print(f"  RS decode succeeded on merged stream.")
        except reedsolo.ReedSolomonError as e:
            print(f"  Merged RS failed: {e}")

    # Strategy C: audio only (only useful if audio covers the whole file, rare)
    if payload is None and len(audio_bytes) > 0:
        audio_trimmed = audio_bytes[: (len(audio_bytes) // rs_block_size) * rs_block_size]
        if audio_trimmed:
            print(f"  Trying audio-only...")
            try:
                payload = rs_decode(audio_trimmed)
                print(f"  RS decode succeeded on audio stream.")
            except reedsolo.ReedSolomonError as e:
                print(f"  Audio-only RS failed: {e}")

    if payload is None:
        sys.exit(
            "\nAll RS decode strategies failed.\n"
            "Possible causes:\n"
            "  - Video was not downloaded at 1080p (use yt-dlp -f 137 or bestvideo[height=1080])\n"
            "  - Too many frames corrupted beyond RS recovery capacity\n"
            "  - Video was re-encoded more than once (e.g. downloaded as 360p then upscaled)\n"
            "  - Wrong encoder settings (BLOCK_SIZE, RS_ECC_SYMBOLS mismatch)"
        )

    # Step 5: extract file
    print(f"\n[5/5] Extracting file from payload ({len(payload):,} bytes)...")
    meta, file_data = parse_payload(payload)

    sha256_computed = hashlib.sha256(file_data).hexdigest()
    sha256_match = sha256_computed == meta["sha256"]

    out_path = os.path.join(output_dir, meta["filename"])
    Path(out_path).write_bytes(file_data)

    print(f"\n  Filename:  {meta['filename']}")
    print(f"  Size:      {len(file_data):,} bytes (expected {meta['size']:,})")
    print(f"  SHA256:    {sha256_computed}")
    print(f"  Integrity: {'PASS' if sha256_match else 'FAIL - data is corrupted'}")
    print(f"  Saved to:  {out_path}")

    if not sha256_match:
        sys.exit("\nSHA256 mismatch. The recovered file does not match the original.")

    print(f"\nDone. '{meta['filename']}' recovered successfully.")


if __name__ == "__main__":
    if len(sys.argv) < 2 or len(sys.argv) > 3:
        print(f"Usage: python {sys.argv[0]} <video.mp4> [output_dir]")
        print(f"  <video.mp4>   downloaded YouTube video at 1080p (use yt-dlp)")
        print(f"  [output_dir]  where to write the recovered file (default: current dir)")
        print()
        print(f"  Download tip: yt-dlp -f \"bestvideo[height=1080][ext=mp4]+bestaudio\" <url>")
        sys.exit(1)

    video = sys.argv[1]
    out_dir = sys.argv[2] if len(sys.argv) == 3 else "."
    os.makedirs(out_dir, exist_ok=True)
    decode(video, out_dir)
