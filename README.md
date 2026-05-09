# FrameVault

A research study into extracting free, unlimited, lossless storage from YouTube by encoding arbitrary binary data into video frames and an audio FSK channel.

This is not a product. It is a study.

---

## Concept

YouTube re-encodes every uploaded video using lossy codecs (VP9, H.264, AV1 for video; Opus for audio). Naively storing data in pixel values would be destroyed immediately. This project works around that by designing an encoding layer *on top of* the video that survives re-encoding, rather than trying to prevent it.

Two independent data channels are used simultaneously:

**Video channel** — each frame is a 30×16 grid of 64×64 pixel blocks. Every block is solid black or solid white (1 bit). Large uniform regions are the cheapest thing a DCT codec can possibly encode, so they survive re-encoding with negligible distortion. Thresholding the center of each block (avoiding compression artifacts at block edges) recovers the original bit reliably.

**Audio channel** — data is encoded as FSK (Frequency Shift Keying) tones: four frequencies map to 2-bit symbols at 100 baud. This is the same principle as a dial-up modem. Opus preserves tonal content in the 1–2 kHz range extremely well, so the tones survive re-encoding and can be recovered via FFT.

The two channels degrade via completely different codecs and completely different error patterns. Reed-Solomon ECC is applied to the raw data before splitting across channels, so errors from either channel can be corrected by the other.

---

## How it works

### Encoding pipeline

```
input file
    |
    v
[metadata header]  <-- JSON: filename, size, SHA256
    |
    v
[payload]  =  4-byte header length  +  header JSON  +  raw file bytes
    |
    v
[Reed-Solomon ECC]  +14% overhead, 32 ECC symbols per 255-byte block
    |
    v
[QR metadata frames]  <-- first 1 second (QR metadata for decoder)
    |
    +---------------------------+
    |                           |
    v                           v
[video channel]           [audio channel]
456 bits per frame        4-FSK, 100 baud
57 bytes per frame        25 bytes/sec
1,710 bytes/sec           independent codec path
    |                           |
    +---------------------------+
                |
                v
         [ffmpeg mux]
         H.264 CRF 0 + AAC 320k
         YouTube-ready MP4
```

### Frame layout

Each 1920×1080 frame contains a 30×16 grid of 64×64 pixel blocks:

```
+--------+--------+--------+--------+--------+  ...  +--------+
| SYNC 0 | SYNC 1 | SYNC 2 | SYNC 3 | SYNC 4 |       | SYNC 7 |  <- row 0, cols 0-7:  sync pattern
+--------+--------+--------+--------+--------+       +--------+
| IDX 0  | IDX 1  | IDX 2  |  ...                   | IDX 15 |  <- row 0, cols 8-23: frame index
+--------+--------+--------+                         +--------+
| DATA   | DATA   | DATA   | DATA   | DATA   |  ...  | DATA   |  <- remaining 456 blocks: data
+--------+--------+--------+--------+--------+       +--------+
| ...    | ...                                       | ...    |
+--------+                                           +--------+
```

- Sync pattern: `10101100` (8 bits, fixed). Used to validate frames and reject corrupted ones.
- Frame index: 16-bit big-endian integer. Supports up to 65,535 frames (~36 minutes at 30fps).
- Data: 456 bits = 57 bytes of Reed-Solomon encoded payload per frame.

The first 1 second (`METADATA_DURATION_SEC`) in the video is reserved for QR metadata
and does **not** contain data blocks. The decoder uses these frames to learn the
expected ECC length, frame count, and SHA256 before assembling payload data.

Each block is sampled at its center 32×32 region (the inner half, margin = 16px). Block edges are where DCT compression artifacts accumulate; the center is clean.

### Audio channel

```
Baud rate:       100 symbols/sec
Bits per symbol: 2  (4-FSK)
Data rate:       25 bytes/sec
Frequencies:     1000 Hz = 00
                 1200 Hz = 01
                 1400 Hz = 10
                 1600 Hz = 11
```

Tones are generated in batch via vectorized numpy, one symbol per 441 samples at 44100 Hz. The audio carries as many bytes of the ECC stream as the video duration allows, starting from byte 0. For small files this covers the entire payload; for large files it covers a prefix that provides a second decode path if video frames fail.

### Decoding pipeline

```
downloaded MP4  (1080p, from yt-dlp)
    |
    +---------------------------+
    |                           |
    v                           v
[video decode]            [audio decode]
ffmpeg pipe               ffmpeg PCM pipe
rgb24 raw frames          44100Hz s16le mono
    |                           |
    v                           v
decode QR metadata        batch FFT per
from first 1 second       441-sample window
    |                           |
    v                           v
expected length           argmax over
frames + SHA256           4 target bins
    |
    v
center-sample
each 64x64 block
threshold at 128
    |
    v
sync check
frame index                 bit stream
    |                           |
    v                           v
sort by index             pack to bytes
fill gaps with 0s
    |
    +---------------------------+
                |
                v
        [merge strategies]
        1. merged  (audio overrides first N bytes; independent error profiles)
        2. video-only
        3. audio-only
                |
                v
        [Reed-Solomon decode]
                |
                v
        [parse payload header]
        extract filename, size, SHA256
                |
                v
        write file + verify SHA256
```

### Why two channels?

H.264/VP9 and Opus fail in structurally different ways:

- H.264/VP9 errors cluster at high-motion boundaries, complex textures, and bitrate-starved regions. A frame with lots of variation near a data block could corrupt that block.
- Opus errors distribute differently: it uses MDCT and a perceptual model tuned for voice and music. Mid-range tones (1–2 kHz) survive very well, but timing artifacts and transient distortion are different from video blocking artifacts.

A byte corrupted in the video channel has no correlation with whether the same byte survived in the audio channel. RS error correction gets to correct against the union of both error sets, not the intersection, which roughly doubles effective correction capacity for the bytes audio covers.

---

## Requirements

- Python 3.10+
- ffmpeg with libx264 (in PATH)
- yt-dlp (for downloading encoded videos back from YouTube)

```
pip install reedsolo numpy qrcode opencv-python-headless
```

---

## Usage

### Encode

```
python yt_encode.py <input_file> <output.mp4>
```

Example:

```
python yt_encode.py archive.zip archive.zip.mp4
```

The output is a standard H.264/AAC MP4 file ready to upload to YouTube. Upload it at the highest resolution offered (1080p or above).

### Decode

```
python yt_decode.py <video.mp4> [output_dir]
```

Download the video from YouTube first using yt-dlp:

```
yt-dlp -f "bestvideo[height=1080][ext=mp4]+bestaudio" https://youtu.be/YOUR_ID -o downloaded.mp4
python yt_decode.py downloaded.mp4 ./recovered/
```

The decoder will print which RS decode strategy succeeded and verify the SHA256 of the recovered file against the stored hash. If SHA256 does not match, it exits non-zero.

### Local round-trip test (no YouTube)

To verify the codec works before uploading:

```
python yt_encode.py myfile.bin myfile.mp4
python yt_decode.py myfile.mp4 ./recovered/
```

This tests the full encode/decode cycle locally. If this fails, the issue is in the codec, not YouTube's re-encoding.

---

## Capacity

| Parameter | Value |
|---|---|
| Frame dimensions | 1920 × 1080 |
| Block size | 64 × 64 px |
| Grid | 30 × 16 = 480 blocks/frame |
| Header bits/frame | 24 (8 sync + 16 index) |
| Data bits/frame | 456 |
| Data bytes/frame | 57 |
| Video data rate | 1,710 bytes/sec at 30fps |
| Audio data rate | 25 bytes/sec (4-FSK, 100 baud) |
| ECC overhead | ~14% (RS-32 over GF(2^8)) |
| Net video throughput | ~1,500 bytes/sec after ECC |
| Max video duration | ~36 min (16-bit frame index) |
| Max file size (video) | ~3.2 GB before ECC |

Audio capacity is 1.5% of video capacity at these settings. For small files (under ~1.5 KB after ECC), audio covers 100% of the payload and provides full redundancy. For larger files it covers a prefix.

To increase audio coverage: raise `BAUD_RATE` in both scripts. 200 baud doubles coverage with minor reliability tradeoff; test post-Opus survival before committing.

---

## Known limitations

**Frame index ceiling.** The 16-bit frame index supports 65,535 frames, which is ~36 minutes at 30fps and ~3.2 GB of raw file data after ECC. For larger files, increase `FRAME_INDEX_BITS` to 24 or 32 in both scripts and adjust `HEADER_BITS` and `DATA_BITS_PER_FRAME` accordingly.

**YouTube re-encoding is untested.** The local round-trip works. YouTube's actual VP9/H.264 output has not yet been tested against this codec. The 64×64 block size was chosen conservatively for this reason. If YouTube's encoder corrupts blocks, the first thing to try is increasing `BLOCK_SIZE` to 128.

**ECC is RS (with erasures for missing frames).** RS handles both errors and erasures. The decoder already treats bytes from sync-failed frames as erasures (known-missing positions) when calling `reedsolo`, which improves recovery vs. pure error correction. Corruption inside frames that still pass sync is still handled as errors.

**Audio coverage is partial for large files.** Audio only carries the first N bytes of the ECC stream. For files where the video channel has widespread failures and audio doesn't cover the affected range, recovery fails. Full redundancy would require a second pass or interleaved encoding.

**reedsolo is pure Python.** For files above ~10 MB, the ECC step is slow. Consider using a compiled RS library or chunking the work for large files.

---

## File structure

```
yt_encode.py    encode any file into a YouTube-ready dual-channel video
yt_decode.py    decode a downloaded video back to the original file
README.md       this file
```

---

## Payload format (reference)

All encoded videos carry a self-describing payload. No external metadata file is needed.

```
Payload layout (before ECC):
+-------------------+---------------------------+-------------------+
| 4 bytes (big-end) | N bytes                   | remaining bytes   |
| metadata length   | UTF-8 JSON metadata        | raw file bytes    |
+-------------------+---------------------------+-------------------+

Metadata JSON fields:
  v          encoding version (integer, currently 2)
  filename   original filename (string)
  size       original file size in bytes (integer)
  sha256     SHA256 hex digest of the original file bytes (string)
```

The RS-encoded payload is then split into 456-bit chunks, one chunk per video frame. The audio channel carries the RS-encoded bytes (not the raw payload) starting from byte 0.

## QR metadata (first 1 second)

The first 1 second (`METADATA_DURATION_SEC`) is QR codes that carry compact metadata for the decoder:

```
{
  "v": 2,              // encoding version
  "f": "file.bin",     // filename
  "s": 12345,          // size in bytes
  "h": "sha256...",    // SHA256 hex
  "e": 67890,          // ECC stream length in bytes
  "n": 42,             // data frame count
  "m": 5               // metadata frame count
}
```

---

## Why this is free and not abuse

YouTube does not charge for storage or bandwidth on uploaded videos. There is no rate limit on uploads for verified accounts. No free trial is being exploited. The videos are valid H.264/AAC MP4 files and conform to YouTube's technical upload requirements. Whether YouTube's terms of service cover this use case is a separate question outside the scope of this study.
