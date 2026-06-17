# FrameVault

A research study into extracting free, lossless storage from YouTube by encoding arbitrary
binary data into video frames that survive re-encoding.

This is not a product. It is a study. The codec is implemented in **pure Rust** (the original
Python prototype lives in git history).

---

## Concept

YouTube re-encodes every uploaded video using lossy codecs (VP9, H.264, AV1). Naively storing
data in pixel values would be destroyed immediately. This project works around that by
designing an encoding layer *on top of* the video that survives re-encoding, rather than
trying to prevent it.

Each frame is a 30×16 grid of 64×64 pixel blocks. Every block is solid black or solid white
(1 bit). Large uniform regions are the cheapest thing a DCT codec can possibly encode, so they
survive re-encoding with negligible distortion. Thresholding the center of each block (avoiding
compression artifacts at block edges) recovers the original bit reliably.

Reed-Solomon ECC is applied to the data before it is laid into frames, so blocks corrupted by
re-encoding — or whole frames dropped entirely — can be reconstructed on decode.

> **Earlier versions also carried a parallel audio 4-FSK channel.** It has been removed: it
> capped throughput, and (because the audio buffer scaled with video *duration*) it was the
> single largest memory cost in the pipeline. FrameVault is now video-only and streams, so
> peak memory no longer grows with file size. Videos produced by versions ≤ 4 are **not**
> decodable by this version.

---

## How it works

### Encoding pipeline

The encoder makes **two streaming passes** over the input and never holds the whole file (or
the whole ECC stream) in memory:

```
input file
   |
   |-- pass 1: stream SHA-256 in 64 KB chunks + stat size
   |             |
   |             v
   |        metadata { v, filename, size, sha256 }
   |             |
   |             v
   |        plan layout (a pure function of file size + filename):
   |        ECC length, data-frame count, frame-index width
   |
   '-- pass 2: logical payload = [4-byte len | meta JSON | raw file bytes]
                 |   (chained via io::Read, not concatenated in memory)
                 v
           StreamingEcc  -- RS(255,223), block-aligned batches -->  ECC byte stream
                 |
                 v
           pack into frames (57 data bytes/frame at 16-bit index)
                 |
                 v   first 1 second = QR metadata frames
           YUV420P luma planes  -->  libav H.264 (CRF 0, ultrafast)  -->  MP4
```

Because every layout value (ECC length, frame count, index width) is a pure function of the
file size, pass 1 only needs the SHA-256; pass 2 then re-opens the file and pipes
`[header | file bytes]` through `StreamingEcc`, which RS-encodes fixed-size batches aligned to
the 223-byte block boundary. That alignment guarantees the streamed ECC bytes are identical to
encoding the whole payload at once. Frames are filled with **packed bytes** directly (no
one-bit-per-byte expansion) and handed to the H.264 encoder as YUV420P luma planes (black =
16, white = 235, neutral chroma) — no intermediate RGB buffer or RGB→YUV scaling pass.

### Frame layout

Each 1920×1080 frame contains a 30×16 grid of 64×64 pixel blocks:

```
+--------+--------+--------+--------+--------+  ...  +--------+
| SYNC 0 | SYNC 1 | SYNC 2 | SYNC 3 | SYNC 4 |       | SYNC 7 |  <- row 0, cols 0-7:  sync pattern
+--------+--------+--------+--------+--------+       +--------+
| IDX 0  | IDX 1  | IDX 2  |  ...                   | IDX 15 |  <- frame index (16-bit default; 32-bit spans cols 8-39)
+--------+--------+--------+                         +--------+
| DATA   | DATA   | DATA   | DATA   | DATA   |  ...  | DATA   |  <- remaining 456 blocks: data
+--------+--------+--------+--------+--------+       +--------+
```

- **Sync pattern:** `10101100` (8 bits, fixed). Validates frames and rejects corrupted ones.
- **Frame index:** big-endian integer, 16-bit by default; the encoder switches to 32-bit when
  the payload needs more than 65,536 frames.
- **Data:** 456 bits (16-bit index) or 440 bits (32-bit index) of Reed-Solomon-encoded payload
  per frame. The data region is always byte-aligned, so frames carry whole packed bytes
  (57 or 55 per frame).

The first 1 second is reserved for QR metadata and contains **no** data blocks. The decoder
uses these frames to learn the expected ECC length, frame count, frame-index width, and SHA256
before assembling payload data.

Each block is sampled at its center 32×32 region (margin = 16 px). Block edges are where DCT
compression artifacts accumulate; the center is clean.

### Decoding pipeline

```
downloaded MP4 (1080p, e.g. from yt-dlp)
   |
   v
libav video decode  -->  per frame: sample block centers, threshold @ 128
   |
   v
read QR metadata (first 1 s): ECC length, frame count, index width, SHA256
   |
   v
sync-check + frame index  -->  write packed bytes straight into one
                               preallocated ECC buffer (a `seen` bitset marks
                               which frames arrived; missing ones become erasures)
   |
   v
Reed-Solomon decode  -->  parse payload  -->  write file + verify SHA256
```

The decoder opens the container with `ignore_editlist` so every encoded frame is returned. As
soon as the QR metadata resolves the layout, each decoded frame is written directly into the
final ECC buffer — there is no per-frame heap map and no bit-per-byte intermediate. Missing
frames are passed to Reed-Solomon as erasures.

---

## Requirements

- Rust toolchain (stable, edition 2021)
- FFmpeg **development** libraries (libav*), used via the `ffmpeg-next` bindings — **no
  `ffmpeg` subprocess is spawned**. On Debian/Ubuntu:

  ```
  sudo apt install libavcodec-dev libavformat-dev libavutil-dev \
    libavfilter-dev libavdevice-dev libswscale-dev libswresample-dev pkg-config clang
  ```

  `clang`/`libclang` is needed by bindgen at build time; if it isn't auto-detected, set
  `LIBCLANG_PATH` (e.g. `export LIBCLANG_PATH=/usr/lib/llvm-18/lib`). Built and tested against
  FFmpeg 6.1.
- `yt-dlp` (only for downloading encoded videos back from YouTube)

```
cargo build --release
```

### Building on Windows

On Windows the build links FFmpeg (+ libx264) **statically**, so the resulting
`framevault.exe` is a self-contained single binary — no FFmpeg DLLs are shipped alongside it.
The build uses the `x64-windows-static-md` triplet (static libs, dynamic CRT), so the binary
still depends on the Microsoft Visual C++ runtime (`VCRUNTIME140.dll`) — install the
[VC++ redistributable](https://aka.ms/vs/17/release/vc_redist.x64.exe) if it isn't already
present. The static, MSVC-compatible libraries are built by
[vcpkg](https://github.com/microsoft/vcpkg).

1. Install [LLVM](https://releases.llvm.org/) (provides `libclang.dll` for bindgen).
2. From the repo root, run:

   ```powershell
   powershell -ExecutionPolicy Bypass -File scripts/setup-windows.ps1
   cargo build --release
   ```

The script clones + bootstraps vcpkg (under `%USERPROFILE%\vcpkg`) and builds
`ffmpeg[x264]:x64-windows-static-md` (a **slow, one-time, from-source** build — GPL/libx264 is
required because the encoder uses H.264 CRF 0; vcpkg auto-acquires nasm/cmake/ninja). FFmpeg is
pinned to **6.1.1** to match the `ffmpeg-next = "6.1"` bindings (FFmpeg 7 removed the
channel-layout API this code uses) via a committed overlay port at
`scripts/vcpkg-overlay/ffmpeg`, layered on vcpkg's current baseline so x264 and the build tools
still come from live mirrors. The script then finds your libclang and writes
`.cargo/config.toml` with the resolved `VCPKG_ROOT`, `VCPKGRS_TRIPLET`, and `LIBCLANG_PATH`.
The `static` feature is enabled for Windows via a `[target.'cfg(windows)']` dependency in
`Cargo.toml`, so non-Windows builds keep linking FFmpeg dynamically. Re-run the script on each
machine — `.cargo/config.toml` holds machine-specific absolute paths.

> **Note:** the committed `.cargo/config.toml` is Windows-specific (its `[env]` paths apply on
> every platform). When building on Linux/macOS, delete it first — the system
> `pkg-config`/`libclang` setup described above needs no config file.

---

## Usage

### Encode

```
cargo run --release -- encode <input_file> <output.mp4>
# or, after building:
./target/release/framevault encode <input_file> <output.mp4>
```

The output is a standard H.264 MP4 ready to upload to YouTube at 1080p or higher.

### Decode

```
./target/release/framevault decode <video.mp4> [output_dir]
```

Download the video from YouTube first using yt-dlp:

```
yt-dlp -f "bestvideo[height=1080][ext=mp4]" https://youtu.be/YOUR_ID -o downloaded.mp4
./target/release/framevault decode downloaded.mp4 ./recovered/
```

The decoder verifies the SHA256 of the recovered file against the stored hash, exiting
non-zero on mismatch.

### Local round-trip test

```
./target/release/framevault encode myfile.bin myfile.mp4
./target/release/framevault decode myfile.mp4 ./recovered/
```

---

## Performance & memory

Frames are generated directly as YUV420P (no RGB buffer / swscale pass), Reed-Solomon runs
across CPU cores via `rayon`, and the whole pipeline streams.

**Peak memory is bounded — it does not grow with file size.** The encoder holds only fixed
buffers (a 64 KB hashing buffer, a ~57 KB / ~65 KB RS batch, and a couple of ~2 MB frame
planes); the rest of the resident set is the constant libx264/libav working set.

Measured on this machine (release build, random input, encode peak RSS):

| Input  | Encode peak RSS |
|--------|----------------:|
| 256 KB | 117 MB          |
| 1 MB   | 121 MB          |
| 3 MB   | 123 MB          |

A full 3 MB round-trip (3,145,728 bytes — near the 16-bit ceiling: 3.43 MiB ECC, 63,141 frames,
~35 min of 1080p video, 624 MB MP4):

| Stage  | Time  | Peak RSS | Result          |
|--------|------:|---------:|-----------------|
| Encode | 137 s | 123 MB   | —               |
| Decode | 184 s | 77 MB    | SHA256 PASS, byte-identical |

(For comparison, the removed audio path would have allocated a duration-sized PCM buffer —
roughly **900 MB** for a 2,100 s track — on top of the encode.)

---

## Capacity

| Parameter | Value |
|---|---|
| Frame dimensions | 1920 × 1080 |
| Block size | 64 × 64 px |
| Grid | 30 × 16 = 480 blocks/frame |
| Data bytes/frame | 57 (16-bit index) / 55 (32-bit index) |
| Video data rate | 1,710 bytes/sec (16-bit) / 1,650 bytes/sec (32-bit) |
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
  rs.rs          Reed-Solomon ECC (reed-solomon crate, rayon-parallel, erasures) + ecc_len_for
  frame.rs       frame layout, index<->bits, packed-byte luma rendering, block sampling
  stream.rs      StreamingEcc: payload Read -> block-aligned RS ECC byte stream
  qr.rs          QR metadata frame generation (qrcode) + decode (rqrr)
  metadata.rs    streaming hash, payload framing + QR metadata (serde)
  media.rs       libav encode (mux) / decode (demux) pipeline, video-only
  encode.rs      two-pass streaming encode pipeline + report
  decode.rs      streaming decode pipeline (video-only RS) + report
  main.rs        clap CLI
tests/
  helpers.rs     codec-helper unit tests (frame-index selection, RS round-trips)
  roundtrip.rs   real encode -> MP4 -> decode -> verify
  media_spike.rs low-level libav round-trip
```

Run the suite with `cargo test` (the heavy 100 KB round-trip is `#[ignore]`d; run it with
`cargo test --release -- --ignored`). `stream.rs` includes a test asserting the streamed ECC
output is byte-for-byte identical to encoding the whole payload at once.

---

## Payload format (reference)

```
Payload layout (before ECC):
+-------------------+---------------------------+-------------------+
| 4 bytes (big-end) | N bytes                   | remaining bytes   |
| metadata length   | UTF-8 JSON metadata       | raw file bytes    |
+-------------------+---------------------------+-------------------+

Metadata JSON fields: v (version), filename, size, sha256
```

## QR metadata (first 1 second)

```
{ "v":5, "f":"file.bin", "s":12345, "h":"sha256...", "e":67890,
  "n":42, "i":16, "m":30 }
```

(version, filename, size, sha256, ECC length, data-frame count, index width, metadata-frame
count). Long-key aliases are also accepted on decode.

---

## Known limitations

- **YouTube re-encoding is untested.** The local round-trip works; YouTube's actual VP9/H.264
  output has not yet been tested against this codec. The 64×64 block size was chosen
  conservatively for this reason.
- **No backward compatibility.** Videos from versions ≤ 4 (which carried an audio channel and
  used metadata version 4) cannot be decoded by this version.
- **QR detection is single-pass** (`rqrr`), unlike the prototype's multi-scale OpenCV + pyzbar
  fallback. For clean local round-trips this is sufficient; if QR metadata is missing entirely,
  the decoder falls back to the frame count it observes and the SHA in the payload header.

---

## Why this is free and not abuse

YouTube does not charge for storage or bandwidth on uploaded videos. The output files are valid
H.264 MP4s conforming to YouTube's technical upload requirements. Whether YouTube's terms of
service cover this use case is a separate question outside the scope of this study.
