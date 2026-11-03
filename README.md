# FrameVault

A research study into extracting free, lossless storage from YouTube by encoding arbitrary
binary data into video frames that survive re-encoding.

This is not a product. It is a study. The codec is implemented in **pure Rust** (the original
Python prototype lives in git history).

It runs two ways, from the same codec:

- **CLI** for Windows, Linux and macOS. Download it from
  [Releases](https://github.com/Advik-B/FrameVault/releases) or [build it](#requirements).
- **In the browser**: the codec compiled to WebAssembly, with
  [ffmpeg.wasm](https://github.com/ffmpegwasm/ffmpeg.wasm) for the video. Try the
  [live demo](https://advik-b.github.io/FrameVault/), or see
  [In the browser](#in-the-browser-webassembly).

Videos made by either one decode in the other.

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
sync-check + frame index  -->  write packed bytes into a block-aligned
                               sliding window over the ECC stream
                               (sized by --memory, not file size)
   |
   v   window fills (or stream ends)
Reed-Solomon decode the window (unseen bytes = erasures)
   |
   v
stream plaintext to the recovered file  -->  rotate window forward
   |
   v
verify SHA256 against the QR-supplied hash
```

The decoder opens the container with `ignore_editlist` so every encoded frame is returned. As
soon as the QR metadata resolves the layout, each decoded frame is written into a sliding
window sized by the memory budget (`--memory`, default 25% of system RAM) rather than into a
buffer sized to the whole file. A window is Reed-Solomon decoded and its plaintext streamed
straight to disk as soon as it fills, so neither the ECC stream nor the recovered file is ever
held whole in memory. Missing bytes become erasures scoped to whichever window they land in.

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

The FFmpeg-dependent parts (libav I/O, the CLI) sit behind the default `native` feature. The
codec core builds and tests without FFmpeg at all:

```
cargo test --no-default-features   # RS, frames, QR, streaming encode/decode: no libav needed
```

### Building on Windows

On Windows the build links FFmpeg (+ libx264, libx265) **statically**, so the resulting
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
`ffmpeg[x264,x265]:x64-windows-static-md` (a **slow, one-time, from-source** build — GPL
libx264/libx265 are required for the lossless H.264 and H.265 encoders; vcpkg auto-acquires
nasm/cmake/ninja). An FFmpeg built by an older version of the script (without x265) is rebuilt
automatically. FFmpeg is
pinned to **6.1.1** to match the `ffmpeg-next = "6.1"` bindings (FFmpeg 7 removed the
channel-layout API this code uses) via a committed overlay port at
`scripts/vcpkg-overlay/ffmpeg`, layered on vcpkg's current baseline so x264 and the build tools
still come from live mirrors. The script then finds your libclang and writes
`.cargo/config.toml` with the resolved `VCPKG_ROOT`, `VCPKGRS_TRIPLET`, and `LIBCLANG_PATH`.
The `static` feature is enabled for Windows via a `[target.'cfg(windows)']` dependency in
`Cargo.toml`, so non-Windows builds keep linking FFmpeg dynamically. Re-run the script on each
machine — `.cargo/config.toml` holds machine-specific absolute paths.

> **Note:** the committed `.cargo/config.toml` is Windows-specific (its `[env]` paths apply on
> every platform). When building on Linux/macOS, delete it first, or set `LIBCLANG_PATH` in your
> environment, which takes precedence (CI does the latter). The system
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

#### Codec and preset

The video is always **lossless**. Two options decide how big the MP4 is and how long encoding
takes; neither affects what decodes:

- `--preset <ultrafast|superfast|veryfast|faster|fast|medium|slow|slower|veryslow>` (default
  `fast`): the x264/x265 speed preset.
- `--codec h265`: lossless H.265/HEVC instead of H.264, for workflows that want HEVC. For
  FrameVault's block frames it's slower than H.264 at every preset, and its files are no
  smaller.

Measured on the 239 frames of a 10 KB input:

| Preset      | H.264 size | H.265 size | Encode time (vs `ultrafast`) |
| ----------- | ---------- | ---------- | ---------------------------- |
| `ultrafast` | 2.3 MB     | 4.9 MB     | 1×                           |
| `fast`      | 262 KB     | 333 KB     | ~1.5×                        |
| `medium`    | 185 KB     | 219 KB     | ~1.8×                        |
| `veryslow`  | 178 KB     | 189 KB     | ~6.5×                        |

```
./target/release/framevault encode big.iso big.mp4 --preset medium
./target/release/framevault encode clip.bin clip.mp4 --codec h265 --preset slow
```

YouTube re-encodes every upload either way, so this mostly saves local disk and upload time.
Decode detects the codec by itself. The browser demo uses `ultrafast`, because slower presets
cost far more in WebAssembly.

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

### Memory budget

Both commands accept `--memory <SIZE>` (e.g. `--memory 512M`, `--memory 2G`, `--memory 1024K`)
to cap how much RAM Reed-Solomon batching (encode) or windowing (decode) is allowed to use.
Encode's batch size and decode's window size are independent — they don't need to match each
other, or match across machines/versions. When omitted, the default is **25% of total system
RAM** (floored at 64 MiB):

```
./target/release/framevault encode big.iso big.mp4 --memory 256M
./target/release/framevault decode big.mp4 ./recovered/ --memory 256M
```

### Local round-trip test

```
./target/release/framevault encode myfile.bin myfile.mp4
./target/release/framevault decode myfile.mp4 ./recovered/
```

---

## In the browser (WebAssembly)

`web/` is a browser app that encodes and decodes FrameVault videos entirely client-side:
nothing is uploaded. It runs the **same Rust codec** as the CLI, compiled to WebAssembly
(`wasm/`, via wasm-bindgen), and uses **ffmpeg.wasm** (the multi-threaded core, with libx264 and
libx265) for the MP4 side. The demo is styled with the Purrimer design system (Blanc and Noir
themes).

```
                 Rust codec (wasm)                     ffmpeg.wasm
encode:  file -> FrameEncoder -> raw yuv420p frames -> libx264/libx265 -> MP4 parts -> concat
decode:  MP4 -> seek + decode a chunk -> 8-bit luma -> FrameDecoder -> file (+ SHA-256 check)
```

Each frame carries only 57 bytes, but a raw 1080p frame is 3 MB, so the browser never holds a
whole video's raw frames:

- **Encode** pipes 48 frames at a time through x264/x265 and concatenates the compressed parts.
- **Decode** seeks through the MP4 96 frames at a time and passes only the luma plane (2 MB per
  frame). Each frame carries its own index, so the couple of frames re-read at each chunk seam
  are harmless.

### Build and run

Requires Rust with the `wasm32-unknown-unknown` target,
[wasm-pack](https://github.com/drager/wasm-pack) and Node 20+.

```
rustup target add wasm32-unknown-unknown
cd web
npm install
npm run build:wasm     # wasm-pack build ../wasm -> web/pkg
npm run dev            # http://localhost:5173
npm run build          # static site in web/dist
```

### Notes

- **Threads need cross-origin isolation.** ffmpeg.wasm's multi-threaded core uses
  `SharedArrayBuffer`, which browsers only expose when the page is served with
  `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp`.
  The Vite dev and preview servers send both. For static hosts that can't, such as GitHub
  Pages, `web/public/coi-serviceworker.js` adds them via a service worker.
- **ffmpeg.wasm is served as-is** from `web/public/ffmpeg/`, copied from `node_modules` before
  each dev or build run. In dev, Vite would otherwise rewrite the core's classic pthread worker
  and break every ffmpeg thread. ffmpeg's thread counts are also capped, because "auto" can ask
  for more threads than ffmpeg.wasm's fixed pthread pool holds, which deadlocks it.
- **Speed.** Small files take seconds. Very large files are better done with the CLI, which is
  several times faster and isn't bound by a browser tab's memory.
- **H.265 previews.** Not every browser can play HEVC in a `<video>` element (Firefox can't), but
  the file is still valid, and the demo can still decode it.

The `wasm/` package can also be used on its own. It exports `Encoder` (a file to raw yuv420p
frames), `Decoder` (rgb24 or luma frames to the file) and the frame geometry helpers. See
`web/src/pipeline.js` for a complete example of driving it with ffmpeg.wasm.

---

## Releases & CI

GitHub Actions (`.github/workflows/`) builds every flavour:

| Workflow      | When                       | What                                                                                                                                         |
| ------------- | -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `ci.yml`      | Every push and PR          | Core tests without FFmpeg; native build, tests and an H.264/H.265 CLI round trip on Linux, macOS and Windows; the wasm package and web demo build |
| `pages.yml`   | Push to `master`           | Deploys the demo to GitHub Pages (set Settings → Pages → Source to "GitHub Actions" once)                                                  |
| `release.yml` | A release is published     | Attaches the assets below to the release                                                                                                     |

Release assets:

- `framevault-windows-x86_64.zip`: a static, self-contained `framevault.exe`, built through
  `scripts/setup-windows.ps1`.
- `framevault-linux-x86_64.tar.gz`: links the system FFmpeg 6.1 (Ubuntu 24.04:
  `sudo apt install ffmpeg`).
- `framevault-macos-arm64.tar.gz`: links Homebrew's FFmpeg 6 (`brew install ffmpeg@6`).
- `framevault-wasm.tar.gz`: the WebAssembly codec package (wasm-pack, `--target web`).
- `framevault-web.zip`: the browser demo, ready for any static host.

The FFmpeg setup for each OS lives in `.github/actions/setup-ffmpeg`. The Windows FFmpeg build
is cached, so only the first run (or a change to the setup script or overlay port) builds it
from source.

---

## Performance & memory

Frames are generated directly as YUV420P (no RGB buffer / swscale pass), Reed-Solomon runs
across CPU cores via `rayon`, and the whole pipeline streams.

**Peak memory is governed by `--memory`, not file size.** Encode batches its RS input and
decode windows its ECC stream, both sized from the same budget (default: 25% of total system
RAM, floored at 64 MiB) — neither buffer ever holds more than one batch/window's worth, no
matter how large the file is. The encoder additionally holds a few small fixed buffers (a
64 KB hashing buffer and a couple of ~2 MB frame planes) on top of its batch; the rest of the
resident set is the constant libx264/libav working set, which dominates peak RSS for any file
small enough to fit in a single batch/window.

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

The same 3 MB round trip at two very different `--memory` settings (32M vs. 512M — a 16x gap
in nominal budget) moves peak RSS by well under 1%, because the allocator only commits pages
the batch/window actually touches — a generous `--memory` costs nothing extra on a file too
small to use it:

| `--memory` | Encode peak RSS | Decode peak RSS | Recovered file |
|-----------:|-----------------:|-----------------:|----------------|
| 32M        | 133 MB            | 81 MB             | SHA256 PASS    |
| 512M       | 133 MB            | 81 MB             | SHA256 PASS    |

The bound itself is proven under stress, not just at a generous budget: `tests/roundtrip.rs`'s
`tiny_memory_budget_round_trip` pushes a 20 KB payload through an explicit 1 KB `--memory`,
forcing dozens of batch/window rotations on both the encode and decode sides through the real
MP4 pipeline — still byte-identical on recovery.

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
  media.rs       libav encode (mux) / decode (demux) pipeline, video-only, H.264/H.265  [native]
  encode.rs      FrameEncoder (I/O-free) + the two-pass file encode pipeline [native]
  decode.rs      FrameDecoder (I/O-free, windowed RS) + the file decode pipeline [native]
  main.rs        clap CLI [native]
tests/
  helpers.rs     codec-helper unit tests (frame-index selection, RS round-trips)
  roundtrip.rs   real encode -> MP4 -> decode -> verify (H.264 and H.265)
  media_spike.rs low-level libav round-trip
wasm/            wasm-bindgen bindings over FrameEncoder / FrameDecoder
web/             browser demo: Vite + React + Purrimer, ffmpeg.wasm for the video
  src/pipeline.js  chunked encode/decode driving the wasm codec and ffmpeg.wasm
scripts/         Windows FFmpeg setup (vcpkg) and the pinned FFmpeg 6.1.1 overlay port
.github/         CI, Pages deploy and release workflows
```

`[native]` marks code behind the default `native` feature, which needs FFmpeg. Everything else
also compiles to `wasm32-unknown-unknown`.

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
