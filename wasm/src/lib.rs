//! wasm-bindgen bindings over FrameVault's I/O-free codec core.
//!
//! The browser side owns the video container: it feeds [`Encoder`]'s raw yuv420p frames
//! to ffmpeg.wasm (libx264 / libx265) and feeds ffmpeg.wasm's decoded rgb24 frames to
//! [`Decoder`]. Everything here is the same code the native CLI runs.

use std::io::{self, Cursor};

use framevault::budget::blocks_for_budget;
use framevault::constants::{FRAME_HEIGHT, FRAME_RATE, FRAME_WIDTH, METADATA_FRAMES};
use framevault::decode::{DecodeStats, FrameDecoder};
use framevault::encode::FrameEncoder;
use framevault::frame::{append_yuv420p, YUV420P_FRAME_BYTES};
use framevault::metadata::build_meta_from_reader;
use wasm_bindgen::prelude::*;

/// Default RS batch/window budget when the caller passes none: small, since the browser
/// already holds the input (or output) file in memory.
const DEFAULT_MEMORY_BYTES: f64 = 16.0 * 1024.0 * 1024.0;

const RGB24_FRAME_BYTES: usize = FRAME_WIDTH * FRAME_HEIGHT * 3;

#[wasm_bindgen(start)]
fn start() {
    console_error_panic_hook::set_once();
}

fn js_err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

fn budget_blocks(memory_bytes: Option<f64>) -> usize {
    blocks_for_budget(memory_bytes.unwrap_or(DEFAULT_MEMORY_BYTES).max(0.0) as u64)
}

/// Crate version.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[wasm_bindgen(js_name = frameWidth)]
pub fn frame_width() -> usize {
    FRAME_WIDTH
}

#[wasm_bindgen(js_name = frameHeight)]
pub fn frame_height() -> usize {
    FRAME_HEIGHT
}

#[wasm_bindgen(js_name = frameRate)]
pub fn frame_rate() -> usize {
    FRAME_RATE
}

#[wasm_bindgen(js_name = metadataFrames)]
pub fn metadata_frames() -> usize {
    METADATA_FRAMES
}

/// Bytes in one raw yuv420p frame (the unit of [`Encoder::next_frames`]'s output).
#[wasm_bindgen(js_name = yuvFrameBytes)]
pub fn yuv_frame_bytes() -> usize {
    YUV420P_FRAME_BYTES
}

/// Bytes in one raw rgb24 frame (the unit of [`Decoder::push_frames`]'s input).
#[wasm_bindgen(js_name = rgbFrameBytes)]
pub fn rgb_frame_bytes() -> usize {
    RGB24_FRAME_BYTES
}

/// Bytes in one raw 8-bit luma frame (the unit of [`Decoder::push_luma_frames`]'s input).
#[wasm_bindgen(js_name = lumaFrameBytes)]
pub fn luma_frame_bytes() -> usize {
    FRAME_WIDTH * FRAME_HEIGHT
}

/// Turns a file into the video's raw frames, in order.
#[wasm_bindgen]
pub struct Encoder {
    inner: FrameEncoder<Cursor<Vec<u8>>>,
    filename: String,
    size: u64,
    sha256: String,
}

#[wasm_bindgen]
impl Encoder {
    /// Plans the video for `data` (named `filename`). `memoryBytes` sizes the
    /// Reed-Solomon streaming batch (default 16 MiB).
    #[wasm_bindgen(constructor)]
    pub fn new(
        filename: String,
        data: Vec<u8>,
        memory_bytes: Option<f64>,
    ) -> Result<Encoder, JsError> {
        let size = data.len() as u64;
        let meta = build_meta_from_reader(&filename, size, &data[..]).map_err(js_err)?;
        let sha256 = meta.sha256.clone();
        let inner = FrameEncoder::new(&meta, Cursor::new(data), budget_blocks(memory_bytes))
            .map_err(js_err)?;
        Ok(Encoder { inner, filename, size, sha256 })
    }

    #[wasm_bindgen(getter)]
    pub fn filename(&self) -> String {
        self.filename.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn size(&self) -> f64 {
        self.size as f64
    }

    #[wasm_bindgen(getter)]
    pub fn sha256(&self) -> String {
        self.sha256.clone()
    }

    #[wasm_bindgen(getter, js_name = totalFrames)]
    pub fn total_frames(&self) -> usize {
        self.inner.plan().total_frames
    }

    #[wasm_bindgen(getter, js_name = dataFrames)]
    pub fn data_frames(&self) -> usize {
        self.inner.plan().num_data_frames
    }

    #[wasm_bindgen(getter, js_name = eccBytes)]
    pub fn ecc_bytes(&self) -> usize {
        self.inner.plan().ecc_len
    }

    #[wasm_bindgen(getter, js_name = indexBits)]
    pub fn index_bits(&self) -> usize {
        self.inner.plan().index_bits
    }

    #[wasm_bindgen(getter, js_name = durationSec)]
    pub fn duration_sec(&self) -> f64 {
        self.inner.plan().duration_sec
    }

    /// Frames emitted so far.
    #[wasm_bindgen(getter)]
    pub fn position(&self) -> usize {
        self.inner.position()
    }

    /// Up to `max` next frames as concatenated raw yuv420p (`yuvFrameBytes()` each).
    /// Empty once every frame has been emitted.
    #[wasm_bindgen(js_name = nextFrames)]
    pub fn next_frames(&mut self, max: usize) -> Result<Vec<u8>, JsError> {
        let remaining = self.inner.plan().total_frames - self.inner.position();
        let n = max.min(remaining);
        let mut out = Vec::with_capacity(n * YUV420P_FRAME_BYTES);
        for _ in 0..n {
            match self.inner.next_luma().map_err(js_err)? {
                Some(luma) => append_yuv420p(&luma, &mut out),
                None => break,
            }
        }
        Ok(out)
    }
}

type OpenFn = fn(&str, u64) -> io::Result<Vec<u8>>;

fn open_in_memory(_filename: &str, size: u64) -> io::Result<Vec<u8>> {
    Ok(Vec::with_capacity(size as usize))
}

/// Studio-swing luma (16..235) to full-range (0..255), rounded and clamped.
fn luma_to_full_range_lut() -> [u8; 256] {
    let mut lut = [0u8; 256];
    for (y, v) in lut.iter_mut().enumerate() {
        *v = ((y as f32 - 16.0) * 255.0 / 219.0).round().clamp(0.0, 255.0) as u8;
    }
    lut
}

/// Recovers a file from the video's decoded frames.
#[wasm_bindgen]
pub struct Decoder {
    inner: Option<FrameDecoder<Vec<u8>, OpenFn>>,
    rgb: Vec<u8>, // reused expansion buffer for `pushLumaFrames`
}

#[wasm_bindgen]
impl Decoder {
    /// `memoryBytes` sizes the Reed-Solomon sliding window (default 16 MiB).
    #[wasm_bindgen(constructor)]
    pub fn new(memory_bytes: Option<f64>) -> Decoder {
        let open: OpenFn = open_in_memory;
        Decoder {
            inner: Some(FrameDecoder::new(budget_blocks(memory_bytes), open)),
            rgb: Vec::new(),
        }
    }

    fn live(&self) -> Option<&FrameDecoder<Vec<u8>, OpenFn>> {
        self.inner.as_ref()
    }

    /// Feed one or more decoded frames (concatenated 1920x1080 rgb24, in stream order).
    /// Repeated frames are harmless. Returns the total frames pushed so far.
    #[wasm_bindgen(js_name = pushFrames)]
    pub fn push_frames(&mut self, rgb: &[u8]) -> Result<usize, JsError> {
        if rgb.len() % RGB24_FRAME_BYTES != 0 {
            return Err(JsError::new(&format!(
                "expected whole rgb24 frames ({RGB24_FRAME_BYTES} bytes each), got {} bytes",
                rgb.len()
            )));
        }
        let dec = self
            .inner
            .as_mut()
            .ok_or_else(|| JsError::new("decoder already finished"))?;
        for frame in rgb.chunks_exact(RGB24_FRAME_BYTES) {
            dec.push_frame(frame).map_err(js_err)?;
        }
        Ok(dec.frames_pushed())
    }

    /// Feed one or more decoded frames as bare luma (concatenated 1920x1080 8-bit Y
    /// planes, e.g. ffmpeg's `extractplanes=y`): a third of rgb24's size. Each frame is
    /// expanded to the rgb24 libav's yuv->rgb conversion yields for neutral chroma
    /// (BT.601 limited -> full range), which is all the block and QR readers sample.
    #[wasm_bindgen(js_name = pushLumaFrames)]
    pub fn push_luma_frames(&mut self, luma: &[u8]) -> Result<usize, JsError> {
        const LUMA_FRAME_BYTES: usize = FRAME_WIDTH * FRAME_HEIGHT;
        if luma.len() % LUMA_FRAME_BYTES != 0 {
            return Err(JsError::new(&format!(
                "expected whole luma frames ({LUMA_FRAME_BYTES} bytes each), got {} bytes",
                luma.len()
            )));
        }
        let dec = self
            .inner
            .as_mut()
            .ok_or_else(|| JsError::new("decoder already finished"))?;
        let lut = luma_to_full_range_lut();
        self.rgb.resize(RGB24_FRAME_BYTES, 0);
        for frame in luma.chunks_exact(LUMA_FRAME_BYTES) {
            for (px, &y) in self.rgb.chunks_exact_mut(3).zip(frame) {
                let v = lut[y as usize];
                px.copy_from_slice(&[v, v, v]);
            }
            dec.push_frame(&self.rgb).map_err(js_err)?;
        }
        Ok(dec.frames_pushed())
    }

    #[wasm_bindgen(getter, js_name = framesPushed)]
    pub fn frames_pushed(&self) -> usize {
        self.live().map_or(0, |d| d.frames_pushed())
    }

    /// Filename from the QR metadata, once read.
    #[wasm_bindgen(getter)]
    pub fn filename(&self) -> Option<String> {
        self.live()?.qr_meta()?.filename.clone()
    }

    /// File size from the QR metadata, once read.
    #[wasm_bindgen(getter)]
    pub fn size(&self) -> Option<f64> {
        Some(self.live()?.qr_meta()?.size? as f64)
    }

    /// Total frames in the video (QR + data) per the QR metadata, once read.
    #[wasm_bindgen(getter, js_name = expectedFrames)]
    pub fn expected_frames(&self) -> Option<f64> {
        let data = self.live()?.qr_meta()?.frames?;
        Some((data + METADATA_FRAMES as u64) as f64)
    }

    /// Flush the final Reed-Solomon window and return the recovered file. A SHA-256
    /// mismatch is reported via `shaOk`, not thrown.
    pub fn finish(&mut self) -> Result<DecodedFile, JsError> {
        let dec = self
            .inner
            .take()
            .ok_or_else(|| JsError::new("decoder already finished"))?;
        let out = dec.finish().map_err(js_err)?;
        Ok(DecodedFile {
            filename: out.filename,
            bytes: out.writer,
            sha256: out.sha256,
            sha_expected: out.sha_expected,
            sha_ok: out.sha_ok,
            stats: out.stats,
        })
    }
}

/// A recovered file.
#[wasm_bindgen]
pub struct DecodedFile {
    filename: String,
    bytes: Vec<u8>,
    sha256: String,
    sha_expected: String,
    sha_ok: bool,
    stats: DecodeStats,
}

#[wasm_bindgen]
impl DecodedFile {
    #[wasm_bindgen(getter)]
    pub fn filename(&self) -> String {
        self.filename.clone()
    }

    /// The file's bytes, copied out to a fresh `Uint8Array`.
    #[wasm_bindgen(getter)]
    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn sha256(&self) -> String {
        self.sha256.clone()
    }

    #[wasm_bindgen(getter, js_name = shaExpected)]
    pub fn sha_expected(&self) -> String {
        self.sha_expected.clone()
    }

    #[wasm_bindgen(getter, js_name = shaOk)]
    pub fn sha_ok(&self) -> bool {
        self.sha_ok
    }

    #[wasm_bindgen(getter, js_name = totalFrames)]
    pub fn total_frames(&self) -> usize {
        self.stats.total
    }

    #[wasm_bindgen(getter, js_name = validFrames)]
    pub fn valid_frames(&self) -> usize {
        self.stats.valid
    }

    #[wasm_bindgen(getter, js_name = missingFrames)]
    pub fn missing_frames(&self) -> usize {
        self.stats.missing
    }
}
