#!/usr/bin/env python3

import sys
import os
from concurrent.futures import ProcessPoolExecutor, ThreadPoolExecutor
import json
import hashlib
import struct
import subprocess
import numpy as np
from pathlib import Path

import reedsolo
try:
    import cv2
except ImportError:  # pragma: no cover - optional for older videos
    cv2 = None

# Must match encoder exactly
FRAME_WIDTH = 1920
FRAME_HEIGHT = 1080
FRAME_RATE = 30
BLOCK_SIZE = 64
COLS = FRAME_WIDTH // BLOCK_SIZE    # 30
ROWS = FRAME_HEIGHT // BLOCK_SIZE   # 16
TOTAL_BLOCKS = COLS * ROWS          # 480

SYNC_PATTERN = np.array([1, 0, 1, 0, 1, 1, 0, 0], dtype=np.uint8)
SYNC_BITS = len(SYNC_PATTERN)
DEFAULT_FRAME_INDEX_BITS = 16
EXTENDED_FRAME_INDEX_BITS = 32

SAMPLE_RATE = 44100
BAUD_RATE = 100
BITS_PER_SYMBOL = 2
SAMPLES_PER_SYMBOL = SAMPLE_RATE // BAUD_RATE       # 441
FSK_FREQS = [1000, 1200, 1400, 1600]

RS_ECC_SYMBOLS = 32
RS_BLOCK_SIZE = 255

# Parallelism
DEFAULT_WORKERS = max(1, os.cpu_count() or 1)
FRAME_BATCH_SIZE = 8
PARALLEL_RS_MIN_BLOCKS = 4
PARALLEL_CHUNK_FACTOR = 4

# Audio layout
AUDIO_LAYOUT_PREFIX = "prefix"
AUDIO_LAYOUT_DISTRIBUTED = "distributed"

# Metadata QR
METADATA_DURATION_SEC = 1.0
METADATA_FRAMES = max(1, int(round(FRAME_RATE * METADATA_DURATION_SEC)))

# How many pixels in from each block edge to sample.
# Block edges are where DCT compression artifacts cluster.
# Center region (64 - 2*16 = 32x32 px) is much cleaner.
BLOCK_SAMPLE_MARGIN = 16
THRESHOLD = 128


def worker_count() -> int:
    raw = os.getenv("FRAMEVAULT_WORKERS")
    if raw:
        try:
            return max(1, int(raw))
        except ValueError:
            pass
    return DEFAULT_WORKERS


def parallel_chunksize(item_count: int, workers: int) -> int:
    return max(1, item_count // max(1, workers * PARALLEL_CHUNK_FACTOR))


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
    luma = frame[:ROWS * BLOCK_SIZE, :COLS * BLOCK_SIZE, 0]  # R channel is sufficient for grayscale frames
    blocks = luma.reshape(ROWS, BLOCK_SIZE, COLS, BLOCK_SIZE)
    centers = blocks[:, m:BLOCK_SIZE - m, :, m:BLOCK_SIZE - m]
    means = centers.mean(axis=(1, 3))
    return (means >= THRESHOLD).astype(np.uint8).reshape(TOTAL_BLOCKS)


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


def decode_frame(bits: np.ndarray, index_bits: int, header_bits: int):
    # Returns (frame_idx, data_bits) or (None, None) if sync check fails.
    sync = bits[:SYNC_BITS]
    if not np.array_equal(sync, SYNC_PATTERN):
        return None, None
    idx_bits = bits[SYNC_BITS:header_bits]
    frame_idx = 0
    for b in idx_bits:
        frame_idx = (frame_idx << 1) | int(b)
    data_bits = bits[header_bits:]
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
    window = np.hanning(SAMPLES_PER_SYMBOL).astype(np.float32)
    windows = windows * window
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


def rs_decode_block(args) -> bytes:
    block, block_erasures = args
    rs = reedsolo.RSCodec(RS_ECC_SYMBOLS)
    if block_erasures:
        decoded, _, _ = rs.decode(block, erase_pos=block_erasures)
    else:
        decoded, _, _ = rs.decode(block)
    return bytes(decoded)


def rs_decode(data: bytes, erasures: list[int] | None = None) -> bytes:
    tasks = []
    for block_start in range(0, len(data), RS_BLOCK_SIZE):
        block = data[block_start:block_start + RS_BLOCK_SIZE]
        if not block:
            break
        if erasures:
            block_erasures = [
                pos - block_start
                for pos in erasures
                if block_start <= pos < block_start + RS_BLOCK_SIZE
            ]
        else:
            block_erasures = None
        tasks.append((block, block_erasures))

    if len(tasks) >= PARALLEL_RS_MIN_BLOCKS and worker_count() > 1:
        rs_workers = min(worker_count(), len(tasks))
        with ProcessPoolExecutor(max_workers=rs_workers) as executor:
            decoded_blocks = list(executor.map(
                rs_decode_block,
                tasks,
                chunksize=parallel_chunksize(len(tasks), rs_workers),
            ))
    else:
        decoded_blocks = [rs_decode_block(task) for task in tasks]
    return b"".join(decoded_blocks)


def parse_payload(payload: bytes, expected_size: int | None = None):
    meta_len = struct.unpack(">I", payload[:4])[0]
    meta = json.loads(payload[4:4 + meta_len])
    file_start = 4 + meta_len
    file_size = expected_size if expected_size is not None else meta["size"]
    file_data = payload[file_start:file_start + file_size]
    if len(file_data) < file_size:
        raise ValueError("Payload ended before expected file size.")
    return meta, file_data


def bits_to_bytes(bits: np.ndarray) -> bytes:
    n = (len(bits) // 8) * 8
    return np.packbits(bits[:n]).tobytes()


def normalize_qr_meta(raw: dict) -> dict:
    def to_int(value):
        return int(value) if value is not None else None

    return {
        "version": to_int(raw.get("v")),
        "filename": raw.get("f") if "f" in raw else raw.get("filename"),
        "size": to_int(raw.get("s") if "s" in raw else raw.get("size")),
        "sha256": raw.get("h") if "h" in raw else raw.get("sha256"),
        "ecc_bytes": to_int(raw.get("e") if "e" in raw else raw.get("ecc_bytes")),
        "frames": to_int(raw.get("n") if "n" in raw else raw.get("frames")),
        "index_bits": to_int(raw.get("i") if "i" in raw else raw.get("index_bits")),
        "metadata_frames": to_int(raw.get("m") if "m" in raw else raw.get("metadata_frames")),
        "audio_bytes": to_int(raw.get("a") if "a" in raw else raw.get("audio_bytes")),
        "audio_layout": raw.get("p") if "p" in raw else raw.get("audio_layout"),
    }


def decode_qr_metadata(frame: np.ndarray, detector) -> dict | None:
    if detector is None:
        return None
    bgr = cv2.cvtColor(frame, cv2.COLOR_RGB2BGR)
    data, _, _ = detector.detectAndDecode(bgr)
    if not data:
        gray = cv2.cvtColor(bgr, cv2.COLOR_BGR2GRAY)
        _, binary = cv2.threshold(gray, 0, 255, cv2.THRESH_BINARY + cv2.THRESH_OTSU)
        data, _, _ = detector.detectAndDecode(binary)
    if not data:
        return None
    try:
        raw = json.loads(data)
    except json.JSONDecodeError:
        return None
    meta = normalize_qr_meta(raw)
    if meta["filename"] is not None and meta["size"] is not None and meta["sha256"] is not None:
        return meta
    return None


def compute_audio_positions(ecc_len: int, audio_byte_count: int, layout: str | None) -> np.ndarray:
    if ecc_len <= 0 or audio_byte_count <= 0:
        return np.empty(0, dtype=np.int64)
    audio_byte_count = min(audio_byte_count, ecc_len)
    if layout in (None, AUDIO_LAYOUT_PREFIX) or audio_byte_count >= ecc_len:
        return np.arange(audio_byte_count, dtype=np.int64)
    step = ecc_len / audio_byte_count
    # Center one sample within each interval so audio redundancy spans the full ECC stream.
    positions = np.floor(np.arange(audio_byte_count, dtype=np.float64) * step + step / 2.0).astype(np.int64)
    return np.clip(positions, 0, ecc_len - 1)


def merge_audio_bytes(
    ecc_video: bytes,
    audio_bytes: bytes,
    expected_audio_bytes: int,
    layout: str | None,
) -> tuple[bytes, np.ndarray]:
    if not audio_bytes or not ecc_video:
        return ecc_video, np.empty(0, dtype=np.int64)
    planned_positions = compute_audio_positions(len(ecc_video), expected_audio_bytes, layout)
    if len(planned_positions) == 0:
        return ecc_video, np.empty(0, dtype=np.int64)
    recovered_positions = planned_positions[:min(len(audio_bytes), len(planned_positions))]
    merged = bytearray(ecc_video)
    for audio_idx, ecc_idx in enumerate(recovered_positions):
        merged[int(ecc_idx)] = audio_bytes[audio_idx]
    return bytes(merged), recovered_positions


def batched_frames(frames, batch_size: int):
    batch = []
    for frame in frames:
        batch.append(frame)
        if len(batch) >= batch_size:
            yield batch
            batch = []
    if batch:
        yield batch


def decode(video_path: str, output_dir: str = "."):
    print(f"Input: {video_path}")

    # Step 1: video channel
    print("\n[1/5] Decoding video channel...")
    index_bits = DEFAULT_FRAME_INDEX_BITS
    header_bits, data_bits_per_frame = frame_layout(index_bits)
    data_bytes_per_frame = data_bits_per_frame // 8
    frames = {}
    total = 0
    sync_fail = 0
    qr_meta = None
    qr_detector = cv2.QRCodeDetector() if cv2 is not None else None
    early_frames = {}
    frame_workers = min(worker_count(), FRAME_BATCH_SIZE)
    if frame_workers > 1:
        print(f"  Parallel frame workers: {frame_workers}")
    with ThreadPoolExecutor(max_workers=frame_workers) as executor:
        for frame_batch in batched_frames(stream_frames(video_path), FRAME_BATCH_SIZE):
            batch_start = total
            for offset, frame_np in enumerate(frame_batch, start=1):
                frame_number = batch_start + offset
                if frame_number <= METADATA_FRAMES and qr_meta is None:
                    qr_meta = decode_qr_metadata(frame_np, qr_detector)
                    if qr_meta:
                        print(f"  QR metadata decoded from frame {frame_number}.")
                        qr_index_bits = qr_meta.get("index_bits")
                        if qr_index_bits is not None:
                            if qr_index_bits not in (DEFAULT_FRAME_INDEX_BITS, EXTENDED_FRAME_INDEX_BITS):
                                print(
                                    f"  Warning: unsupported index width {qr_index_bits}; falling back to "
                                    f"{index_bits}-bit decoding (may fail)."
                                )
                            elif qr_index_bits != index_bits:
                                old_index_bits = index_bits
                                if frames or early_frames:
                                    print(
                                        f"  Warning: unexpected index width change from {old_index_bits} to "
                                        f"{qr_index_bits} bits; this may indicate corrupted or mixed sources. "
                                        "Discarding previously decoded frames."
                                    )
                                    frames = {}
                                    early_frames = {}
                                index_bits = qr_index_bits
                                header_bits, data_bits_per_frame = frame_layout(index_bits)
                                data_bytes_per_frame = data_bits_per_frame // 8

            for bits in executor.map(read_blocks, frame_batch):
                total += 1
                idx, data_bits = decode_frame(bits, index_bits, header_bits)
                if idx is None:
                    sync_fail += 1
                else:
                    if total <= METADATA_FRAMES and qr_meta is None:
                        early_frames[idx] = data_bits
                    else:
                        frames[idx] = data_bits
                if total % 30 == 0:
                    print(f"  {total} frames read, {len(frames)} valid, {sync_fail} sync failures", end="\r", flush=True)

    print()
    print(f"  Total: {total} | Valid: {len(frames)} | Sync failures: {sync_fail}")
    if qr_meta is None:
        frames.update({idx: bits for idx, bits in early_frames.items() if idx not in frames})
        print("  QR metadata not found; falling back to payload header.")
    else:
        if qr_meta.get("metadata_frames") not in (None, METADATA_FRAMES):
            print(f"  QR metadata expects {qr_meta['metadata_frames']} metadata frames.")

    if not frames:
        sys.exit("No valid frames found. Check that the video is 1080p and was encoded with yt_encode.py.")

    # Step 2: reassemble video bits
    print(f"\n[2/5] Reassembling frame data...")
    expected_frames = qr_meta.get("frames") if qr_meta else None
    expected_ecc_bytes = qr_meta.get("ecc_bytes") if qr_meta else None
    if expected_frames is not None:
        frames = {idx: bits for idx, bits in frames.items() if idx < expected_frames}
        frame_count = expected_frames
    else:
        frame_count = max(frames.keys()) + 1

    total_bits = frame_count * data_bits_per_frame
    all_bits = np.zeros(total_bits, dtype=np.uint8)
    missing = []

    for i in range(frame_count):
        if i in frames:
            all_bits[i * data_bits_per_frame:(i + 1) * data_bits_per_frame] = frames[i]
        else:
            missing.append(i)

    if missing:
        print(f"  Missing {len(missing)} frames: {missing[:10]}{'...' if len(missing)>10 else ''}")
        print(f"  Reed-Solomon will attempt recovery ({len(missing)*data_bytes_per_frame} bytes zeroed).")
    else:
        print(f"  All {frame_count} frames present.")

    ecc_from_video = bits_to_bytes(all_bits)
    if expected_ecc_bytes:
        if expected_ecc_bytes <= len(ecc_from_video):
            ecc_from_video = ecc_from_video[:expected_ecc_bytes]
        else:
            print(f"  Warning: QR expects {expected_ecc_bytes:,} bytes, but only {len(ecc_from_video):,} available.")
    print(f"  Video ECC stream: {len(ecc_from_video):,} bytes")

    # Step 3: audio channel
    print(f"\n[3/5] Decoding audio channel (4-FSK @ {BAUD_RATE} baud)...")
    audio_samples = extract_audio(video_path)
    audio_bytes = decode_fsk(audio_samples)
    ecc_len_for_coverage = len(ecc_from_video) if ecc_from_video else 0
    coverage_pct = len(audio_bytes) / ecc_len_for_coverage * 100 if ecc_len_for_coverage else 0
    print(f"  Recovered: {len(audio_bytes):,} bytes ({coverage_pct:.1f}% of ECC stream)")

    # Step 4: RS decode — try strategies in order of confidence
    print(f"\n[4/5] Reed-Solomon decode...")
    payload = None

    # Each RS block is at most 255 bytes. The final block may be shorter.
    # If QR metadata isn't available, trimming to full blocks can avoid
    # trailing frame padding, but keep short payloads intact.
    ecc_video = ecc_from_video
    if expected_ecc_bytes is None and len(ecc_video) >= RS_BLOCK_SIZE:
        ecc_video = ecc_video[: (len(ecc_video) // RS_BLOCK_SIZE) * RS_BLOCK_SIZE]
    missing_byte_positions = [
        i * data_bytes_per_frame + j
        for i in missing
        for j in range(data_bytes_per_frame)
    ]
    erasures_video = [pos for pos in missing_byte_positions if pos < len(ecc_video)]

    # Strategy A: video only — best for local lossless files where video is perfect
    # but audio (AAC→FSK) is noisy. Trying this first avoids overwriting correct
    # video bytes with noisy audio bytes.
    print(f"  Trying video-only ({len(ecc_video):,} bytes)...")
    try:
        payload = rs_decode(ecc_video, erasures=erasures_video if erasures_video else None)
        print(f"  RS decode succeeded on video stream.")
    except reedsolo.ReedSolomonError as e:
        print(f"  Video-only RS failed: {e}")

    # Strategy B: merge audio into video for bytes audio covers, then RS decode.
    # Rationale: audio and video degrade independently (different codecs, different
    # error patterns). Bytes that video mangled may be intact in audio and vice versa.
    # RS then corrects whatever residual errors remain.
    if payload is None and len(audio_bytes) > 0:
        audio_layout = qr_meta.get("audio_layout") if qr_meta else None
        expected_audio_bytes = qr_meta.get("audio_bytes") if qr_meta else len(audio_bytes)
        merged, recovered_positions = merge_audio_bytes(ecc_video, audio_bytes, expected_audio_bytes, audio_layout)
        recovered_position_set = set(int(pos) for pos in recovered_positions.tolist())
        erasures_merged = (
            [pos for pos in erasures_video if pos not in recovered_position_set]
            if erasures_video else None
        )
        print(f"  Trying merged (audio+video, layout={audio_layout or AUDIO_LAYOUT_PREFIX})...")
        try:
            payload = rs_decode(merged, erasures=erasures_merged)
            print(f"  RS decode succeeded on merged stream.")
        except reedsolo.ReedSolomonError as e:
            print(f"  Merged RS failed: {e}")

    # Strategy C: audio only (only useful if audio covers the whole file, rare)
    if payload is None and len(audio_bytes) > 0:
        audio_trimmed = audio_bytes[: (len(audio_bytes) // RS_BLOCK_SIZE) * RS_BLOCK_SIZE]
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
    expected_size = qr_meta["size"] if qr_meta else None
    expected_sha = qr_meta["sha256"] if qr_meta else None
    expected_filename = qr_meta["filename"] if qr_meta else None
    meta, file_data = parse_payload(payload, expected_size=expected_size)

    sha256_computed = hashlib.sha256(file_data).hexdigest()
    sha256_expected = expected_sha or meta["sha256"]
    sha256_match = sha256_computed == sha256_expected

    out_name = expected_filename or meta["filename"]
    out_path = os.path.join(output_dir, out_name)
    Path(out_path).write_bytes(file_data)

    if qr_meta and (
        meta.get("filename") != expected_filename
        or meta.get("size") != expected_size
        or meta.get("sha256") != expected_sha
    ):
        print("  Warning: payload metadata differs from QR metadata.")

    print(f"\n  Filename:  {out_name}")
    expected_size_display = expected_size if expected_size is not None else meta["size"]
    print(f"  Size:      {len(file_data):,} bytes (expected {expected_size_display:,})")
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
