// The browser pipeline: FrameVault's Rust codec (WebAssembly) renders and reads the
// frames; ffmpeg.wasm (multi-threaded core) turns them into an MP4 and back.
//
// A 1080p raw frame is 3 MB (yuv420p) or 2 MB (luma), while each frame carries only 57
// bytes of data, so nothing ever holds a whole video's raw frames. Both directions work
// in bounded chunks: encode pipes CHUNK frames at a time through libx264/libx265 and
// concatenates the compressed parts; decode seeks through the MP4 a chunk at a time.

import { FFmpeg } from "@ffmpeg/ffmpeg";

import initCodec, * as fv from "../pkg/framevault_wasm.js";

// ffmpeg.wasm's multi-threaded core, served as-is from public/ffmpeg/ (see
// scripts/copy-ffmpeg-core.mjs). Absolute URLs: the core's threads resolve them from
// inside workers, where a relative path would point somewhere else.
const coreBase = new URL(`${import.meta.env.BASE_URL}ffmpeg/`, document.baseURI);
const coreURL = new URL("ffmpeg-core.js", coreBase).href;
const wasmURL = new URL("ffmpeg-core.wasm", coreBase).href;
const workerURL = new URL("ffmpeg-core.worker.js", coreBase).href;

const ENCODE_CHUNK = 48; // frames per ffmpeg encode call (~150 MB raw)
const DECODE_CHUNK = 96; // frames per ffmpeg decode call (~200 MB luma)
const DECODE_OVERLAP = 2; // frames re-read at each chunk seam; duplicates are harmless
const PUSH_BATCH = 8; // frames handed to the decoder between UI yields

// ffmpeg.wasm's threads come from a fixed, pre-spawned pool. Left on "auto", libavcodec,
// x264 and x265 size their thread counts from the CPU count and can ask for more threads
// than the pool holds, which deadlocks ffmpeg. Cap them well inside the pool. (x265 keeps
// its own thread pool, which `-threads` doesn't reach: it's bounded via `pools`.)
const THREADS = String(Math.max(1, Math.min(8, navigator.hardwareConcurrency || 4)));

export const CODECS = {
  h264: {
    label: "H.264",
    // Lossless (CRF 0). The browser uses the `ultrafast` preset for speed; the CLI
    // defaults to `fast` (files ~9x smaller), see its --preset option.
    args: ["-c:v", "libx264", "-preset", "ultrafast", "-crf", "0"],
  },
  h265: {
    label: "H.265",
    // x265's CRF 0 isn't lossless; its lossless mode is a separate switch. `hvc1` lets
    // Apple players and browsers recognise the stream.
    args: [
      "-c:v", "libx265", "-preset", "ultrafast",
      "-x265-params", `lossless=1:log-level=error:pools=${THREADS}:frame-threads=2`,
      "-tag:v", "hvc1",
    ],
  },
};

let codecReady;
let ffmpeg;
let ffmpegReady;
let ffmpegLog = [];

/** Load the Rust codec (small, fast). */
export function loadCodec() {
  codecReady ??= initCodec().then(() => ({
    version: fv.version(),
    width: fv.frameWidth(),
    height: fv.frameHeight(),
    fps: fv.frameRate(),
  }));
  return codecReady;
}

/** Load ffmpeg.wasm's multi-threaded core (about 32 MB, cached by the browser). */
export function loadFFmpeg() {
  ffmpegReady ??= (async () => {
    if (!globalThis.crossOriginIsolated) {
      throw new Error(
        "This page isn't cross-origin isolated, so ffmpeg can't use threads. Reload the page; " +
          "if that doesn't help, your browser may not support SharedArrayBuffer.",
      );
    }
    ffmpeg = new FFmpeg();
    ffmpeg.on("log", ({ message }) => {
      ffmpegLog.push(message);
      if (ffmpegLog.length > 200) ffmpegLog = ffmpegLog.slice(-100);
    });
    await ffmpeg.load({ coreURL, wasmURL, workerURL });
    return ffmpeg;
  })();
  ffmpegReady.catch(() => (ffmpegReady = undefined));
  return ffmpegReady;
}

async function run(args) {
  ffmpegLog = [];
  const code = await ffmpeg.exec(["-hide_banner", "-loglevel", "error", "-nostdin", ...args]);
  if (code !== 0) {
    const detail = ffmpegLog.filter(Boolean).slice(-4).join(" ").trim();
    throw new Error(`ffmpeg stopped with code ${code}${detail ? `: ${detail}` : "."}`);
  }
}

async function removeQuietly(path) {
  try {
    await ffmpeg.deleteFile(path);
  } catch {
    /* already gone */
  }
}

const tick = () => new Promise((r) => setTimeout(r, 0));

/** Plan an encode without running it: frames, duration and so on. */
export async function planEncode(file) {
  await loadCodec();
  const data = new Uint8Array(await file.arrayBuffer());
  return new fv.Encoder(file.name, data);
}

/**
 * Encode `encoder` (from planEncode) into an MP4.
 * onProgress({ done, total, phase }) is called as frames are encoded.
 */
export async function encodeVideo(encoder, codecKey, { onProgress, signal } = {}) {
  const ff = await loadFFmpeg();
  const { width, height, fps } = await loadCodec();
  const codec = CODECS[codecKey];
  const total = encoder.totalFrames;
  const frameBytes = fv.yuvFrameBytes();
  const parts = [];

  try {
    while (encoder.position < total) {
      signal?.throwIfAborted();
      const n = Math.min(ENCODE_CHUNK, total - encoder.position);
      // Pull frames one at a time into a JS buffer so wasm memory stays small.
      const raw = new Uint8Array(n * frameBytes);
      for (let i = 0; i < n; i++) raw.set(encoder.nextFrames(1), i * frameBytes);
      await ff.writeFile("chunk.yuv", raw);

      const part = `part${String(parts.length).padStart(5, "0")}.mp4`;
      await run([
        "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", `${width}x${height}`,
        "-framerate", String(fps), "-i", "chunk.yuv",
        ...codec.args, "-threads", THREADS, "-pix_fmt", "yuv420p", "-an", part,
      ]);
      await removeQuietly("chunk.yuv");
      parts.push(part);
      onProgress?.({ done: encoder.position, total, phase: "encode" });
    }

    onProgress?.({ done: total, total, phase: "mux" });
    await ff.writeFile("parts.txt", parts.map((p) => `file '${p}'`).join("\n"));
    await run([
      "-f", "concat", "-safe", "0", "-i", "parts.txt",
      "-c", "copy", ...(codecKey === "h265" ? ["-tag:v", "hvc1"] : []),
      "-movflags", "+faststart", "out.mp4",
    ]);
    const mp4 = await ff.readFile("out.mp4");
    return new Blob([mp4], { type: "video/mp4" });
  } finally {
    await Promise.all(
      [...parts, "chunk.yuv", "parts.txt", "out.mp4"].map(removeQuietly),
    );
  }
}

/**
 * Decode a FrameVault MP4 back into the original file.
 * onProgress({ done, total, filename }) is called as frames are read; `total` and
 * `filename` appear once the QR metadata frames have been read.
 */
export async function decodeVideo(file, { onProgress, signal } = {}) {
  const ff = await loadFFmpeg();
  const { width, height, fps } = await loadCodec();
  const lumaBytes = fv.lumaFrameBytes();
  const decoder = new fv.Decoder();

  // Mount the File read-only rather than copying it into ffmpeg's memory.
  const mountDir = "/input";
  let input;
  let mounted = false;
  try {
    await ff.createDir(mountDir);
    await ff.mount("WORKERFS", { files: [file] }, mountDir);
    mounted = true;
    input = `${mountDir}/${file.name}`;
  } catch {
    input = "input.mp4";
    await ff.writeFile(input, new Uint8Array(await file.arrayBuffer()));
  }

  try {
    let start = 0;
    for (;;) {
      signal?.throwIfAborted();
      // Seek half a frame early so float rounding can't skip the frame at `start`.
      const seek = start > 0 ? ["-ss", ((start - 0.5) / fps).toFixed(6)] : [];
      await run([
        "-threads", THREADS, "-ignore_editlist", "1", ...seek, "-i", input,
        "-frames:v", String(DECODE_CHUNK), "-an", "-filter_threads", "1",
        // Scale to 1080p like the native decoder, then keep only the luma plane.
        "-vf", `scale=${width}:${height}:flags=bilinear,format=yuv420p,extractplanes=y`,
        "-f", "rawvideo", "-pix_fmt", "gray", "chunk.y",
      ]);
      const luma = await ff.readFile("chunk.y");
      await removeQuietly("chunk.y");
      const n = Math.floor(luma.length / lumaBytes);

      for (let i = 0; i < n; i += PUSH_BATCH) {
        const end = Math.min(n, i + PUSH_BATCH);
        decoder.pushLumaFrames(luma.subarray(i * lumaBytes, end * lumaBytes));
        onProgress?.({
          done: Math.min(start + end, decoder.expectedFrames ?? Infinity),
          total: decoder.expectedFrames,
          filename: decoder.filename,
        });
        await tick();
      }
      if (n < DECODE_CHUNK) break;
      start += n - DECODE_OVERLAP;
    }

    const out = decoder.finish();
    const bytes = out.bytes; // one copy out of wasm memory
    const result = {
      filename: out.filename,
      blob: new Blob([bytes]),
      size: bytes.length,
      sha256: out.sha256,
      shaExpected: out.shaExpected,
      shaOk: out.shaOk,
      validFrames: out.validFrames,
      missingFrames: out.missingFrames,
    };
    out.free();
    return result;
  } finally {
    decoder.free();
    if (mounted) {
      try {
        await ff.unmount(mountDir);
        await ff.deleteDir(mountDir);
      } catch {
        /* best effort */
      }
    } else {
      await removeQuietly(input);
    }
    await removeQuietly("chunk.y");
  }
}

/** Whether this browser can play an H.265 MP4 in a <video> element. */
export function canPlayHevc() {
  const v = document.createElement("video");
  return v.canPlayType('video/mp4; codecs="hvc1.1.6.L120.90"') !== "";
}
