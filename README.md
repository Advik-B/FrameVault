# FrameVault

A research study into extracting free, unlimited, lossless storage from YouTube by
encoding arbitrary binary data into video frames and an audio FSK channel.

This is not a product. It is a study. The codec is implemented in **pure Rust** (the
original Python prototype lives in git history).

---

## Concept

YouTube re-encodes every uploaded video using lossy codecs (VP9, H.264, AV1 for video;
Opus for audio). Naively storing data in pixel values would be destroyed immediately.
This project works around that by designing an encoding layer *on top of* the video that
survives re-encoding, rather than trying to prevent it.

Two independent data channels are used simultaneously:

**Video channel** — each frame is a 30×16 grid of 64×64 pixel blocks. Every block is solid
black or solid white (1 bit). Large uniform regions are the cheapest thing a DCT codec can
possibly encode, so they survive re-encoding with negligible distortion. Thresholding the
center of each block (avoiding compression artifacts at block edges) recovers the original
bit reliably.

**Audio channel** — data is encoded as FSK (Frequency Shift Keying) tones: four frequencies
map to 2-bit symbols at 100 baud. This is the same principle as a dial-up modem. Opus
preserves tonal content in the 1–2 kHz range extremely well, so the tones survive
re-encoding and can be recovered via FFT.

The two channels degrade via completely different codecs and completely different error
patterns. Reed-Solomon ECC is applied to the raw data before splitting across channels, so
errors from either channel can be corrected by the other.

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
         [libav mux]
         H.264 CRF 0 + AAC 320k
         YouTube-ready MP4
```

Frames are generated directly as YUV420P luma planes (black = 16, white = 235, neutral
chroma) and handed straight to the H.264 encoder — there is no intermediate RGB buffer or
RGB→YUV scaling pass, which is the bulk of the speed advantage over the prototype.

### Frame layout

Each 1920×1080 frame contains a 30×16 grid of 64×64 pixel blocks:

```
+--------+--------+--------+--------+--------+  ...  +--------+
| SYNC 0 | SYNC 1 | SYNC 2 | SYNC 3 | SYNC 4 |       | SYNC 7 |  <- row 0, cols 0-7:  sync pattern
+--------+--------+--------+--------+--------+       +--------+
| IDX 0  | IDX 1  | IDX 2  |  ...                   | IDX 15 |  <- row 0, cols 8-23: frame index (16-bit default; 32-bit spans cols 8-39)
+--------+--------+--------+                         +--------+
| DATA   | DATA   | DATA   | DATA   | DATA   |  ...  | DATA   |  <- remaining 456 blocks: data
+--------+--------+--------+--------+--------+       +--------+
```

- Sync pattern: `10101100` (8 bits, fixed). Used to validate frames and reject corrupted ones.
- Frame index: big-endian integer, 16-bit by default. Encoder switches to 32-bit when needed (hard limit).
- Data: 456 bits (16-bit index) or 440 bits (32-bit index) of Reed-Solomon encoded payload per frame.

The first 1 second is reserved for QR metadata and does **not** contain data blocks. The
decoder uses these frames to learn the expected ECC length, frame count, frame index width,
and SHA256 before assembling payload data.

Each block is sampled at its center 32×32 region (margin = 16px). Block edges are where DCT
compression artifacts accumulate; the center is clean.

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

The audio carries as many ECC bytes as the video duration allows. Positions use the
**distributed** layout: audio bytes are spaced evenly across the *entire* ECC stream
(position 0 through ecc_len−1), so every audio byte contributes error-correction signal at a
different part of the file.

### Decoding pipeline

```
downloaded MP4  (1080p, from yt-dlp)
    |
    +---------------------------+
    |                           |
    v                           v
[video decode]            [audio decode]
libav frames              libav PCM
1080p luma                44100Hz mono
    |                           |
    v                           v
decode QR metadata        windowed FFT per
center-sample blocks      441-sample symbol
threshold @ 128           argmax over 4 bins
    |                           |
    v                           v
sync check + frame index  bit stream -> bytes
sort by index, fill gaps
    |                           |
    +---------------------------+
                |
                v
        [merge strategies]
        1. video-only  (tried first; fastest path for clean local files)
        2. merged      (audio global parity merged at planned ECC positions)
        3. audio-only  (fallback)
                |
                v
        [Reed-Solomon decode] -> [parse payload] -> write file + verify SHA256
```

The video decode opens the container with `ignore_editlist` so every encoded frame is
returned (the AAC priming delay otherwise adds an edit list that trims the final frame).

### Why two channels?

H.264/VP9 and Opus fail in structurally different ways, and a byte corrupted in the video
channel has no correlation with whether the same byte survived in the audio channel. RS error
correction gets to correct against the union of both error sets, not the intersection.

---

## Requirements

- Rust toolchain (stable, edition 2021)
- FFmpeg **development** libraries (libav*), used via the `ffmpeg-next` bindings — **no
  `ffmpeg` subprocess is spawned**. On Debian/Ubuntu:

  ```
  sudo apt install libavcodec-dev libavformat-dev libavutil-dev \
    libavfilter-dev libavdevice-dev libswscale-dev libswresample-dev pkg-config clang
  ```

  (`clang`/`libclang` is needed by bindgen at build time. Built and tested against FFmpeg 6.1.)
- `yt-dlp` (only for downloading encoded videos back from YouTube)

```
cargo build --release
```

---

## Usage

### Encode

```
cargo run --release -- encode <input_file> <output.mp4>
# or, after building:
./target/release/framevault encode <input_file> <output.mp4>
```

The output is a standard H.264/AAC MP4 ready to upload to YouTube at 1080p or higher.

### Decode

```
./target/release/framevault decode <video.mp4> [output_dir]
```

Download the video from YouTube first using yt-dlp:

```
yt-dlp -f "bestvideo[height=1080][ext=mp4]+bestaudio" https://youtu.be/YOUR_ID -o downloaded.mp4
./target/release/framevault decode downloaded.mp4 ./recovered/
```

The decoder reports which RS strategy succeeded and verifies the SHA256 of the recovered file
against the stored hash, exiting non-zero on mismatch.

### Local round-trip test

```
./target/release/framevault encode myfile.bin myfile.mp4
./target/release/framevault decode myfile.mp4 ./recovered/
```

---

## Performance (Rust vs. the Python prototype)

Wall-clock encode + decode of random files, same codec settings (1080p, H.264 CRF 0
ultrafast, AAC 320k), verified byte-identical both ways. Both implementations ultimately call
the same libx264/AAC encoders (Rust via libav bindings, Python via the ffmpeg CLI); the Rust
gains come from generating YUV420P frames directly (no RGB buffer / swscale pass), compiled
Reed-Solomon, and no Python/numpy/process-pool overhead.

| File size | Rust encode | Rust decode | Python encode | Python decode |
|-----------|------------:|------------:|--------------:|--------------:|
| 1 KB      | 0.58 s      | 0.52 s      | 1.80 s        | 1.62 s        |
| 10 KB     | 2.15 s      | 1.65 s      | 5.03 s        | 2.97 s        |
| 100 KB    | 15.79 s     | 12.44 s     | 46.04 s       | 15.72 s       |

≈ **2.3–3× faster encode** and **1.3–1.8× faster decode**. Profiling the 100 KB encode showed
the prototype spent ~35 s in RGB→YUV scaling alone; generating YUV directly cut that to ~2 s.

---

## Capacity

| Parameter | Value |
|---|---|
| Frame dimensions | 1920 × 1080 |
| Block size | 64 × 64 px |
| Grid | 30 × 16 = 480 blocks/frame |
| Data bits/frame | 456 (16-bit index) / 440 (32-bit index) |
| Video data rate | 1,710 bytes/sec (16-bit) / 1,650 bytes/sec (32-bit) |
| Audio data rate | 25 bytes/sec (4-FSK, 100 baud) |
| ECC overhead | ~14% (RS-32 over GF(2^8)) |

### Maximum file size

| Mode | Index bits | Bytes/frame | Max frames | Max ECC stream | Max raw file |
|------|-----------|-------------|------------|----------------|--------------|
| **16-bit** (default) | 16 | 57 | 65,536 | 3.56 MB | ~3.11 MB |
| **32-bit** (auto-selected) | 32 | 55 | 4,294,967,296 | ~220 GB | ~193 GB |

The encoder automatically selects 16-bit when the payload fits in 65,536 frames; otherwise it
switches to 32-bit (hard limit, ~4.5 years of video at 30 fps).

---

## Project structure

```
src/
  constants.rs   shared codec constants
  rs.rs          Reed-Solomon ECC (reed-solomon crate, rayon-parallel, erasures)
  frame.rs       frame layout, index<->bits, luma rendering, block sampling
  audio.rs       4-FSK modulation + FFT demodulation, audio byte placement
  qr.rs          QR metadata frame generation (qrcode) + decode (rqrr)
  metadata.rs    payload framing + QR metadata (serde)
  media.rs       libav encode (mux) / decode (demux) pipeline
  encode.rs      encode pipeline + report
  decode.rs      decode pipeline (video-only/merged/audio-only) + report
  main.rs        clap CLI
tests/
  helpers.rs     codec-helper unit tests (no media)
  roundtrip.rs   real encode -> MP4 -> decode -> verify
  media_spike.rs low-level libav round-trip
```

Run the suite with `cargo test` (the heavy 100 KB round-trip is `#[ignore]`d; run it with
`cargo test --release -- --ignored`).

---

## Payload format (reference)

```
Payload layout (before ECC):
+-------------------+---------------------------+-------------------+
| 4 bytes (big-end) | N bytes                   | remaining bytes   |
| metadata length   | UTF-8 JSON metadata        | raw file bytes    |
+-------------------+---------------------------+-------------------+

Metadata JSON fields: v (version), filename, size, sha256, audio_layout
```

## QR metadata (first 1 second)

```
{ "v":4, "f":"file.bin", "s":12345, "h":"sha256...", "e":67890,
  "n":42, "i":16, "m":30, "a":250, "p":"distributed" }
```

(version, filename, size, sha256, ECC length, data frame count, index width, metadata frame
count, audio bytes, audio layout). Long-key aliases are also accepted on decode.

---

## Parallel processing

Reed-Solomon block encode/decode runs across CPU cores via `rayon`. The release profile is
tuned for speed (`opt-level = 3`, LTO, single codegen unit).

---

## Known limitations

- **YouTube re-encoding is untested.** The local round-trip works; YouTube's actual VP9/H.264
  output has not yet been tested against this codec. The 64×64 block size was chosen
  conservatively for this reason.
- **Audio coverage is partial for large files.** The audio track samples ECC bytes evenly
  across the whole stream (global parity), but is bandwidth-limited (~25 bytes/sec).
- **QR detection is single-pass** (`rqrr`), unlike the prototype's multi-scale OpenCV + pyzbar
  fallback. For clean local round-trips this is sufficient, and the payload header provides a
  fallback when QR metadata is unavailable.

---

## Why this is free and not abuse

YouTube does not charge for storage or bandwidth on uploaded videos. The output files are
valid H.264/AAC MP4s conforming to YouTube's technical upload requirements. Whether YouTube's
terms of service cover this use case is a separate question outside the scope of this study.
