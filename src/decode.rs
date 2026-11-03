//! Decode pipeline: MP4 -> video block channel -> Reed-Solomon -> original file.
//!
//! Frames stream in one at a time. Once the QR metadata resolves the frame count, index
//! width, filename, size, and SHA-256, each decoded frame's packed bytes are written into
//! a block-aligned sliding [`Window`] over the conceptual ECC byte stream — sized by the
//! memory budget (`--memory`, or a fraction of system RAM), not the file size. A window is
//! Reed-Solomon decoded and its plaintext streamed straight to the output via
//! [`PayloadSink`] as soon as it fills, so neither the ECC stream nor the recovered file
//! is ever held whole in memory. Missing bytes become RS erasures, scoped to whichever
//! window they fall in.
//!
//! If the QR metadata channel never resolves a frame count (rare: a non-FrameVault or
//! badly corrupted video), decode falls back to an end-of-stream path bounded by file
//! size rather than budget, since there's no frame count to size a window against.
//!
//! [`FrameDecoder`] is the I/O-free core (rgb24 frames in, file bytes out to any
//! `io::Write`) shared by the native CLI and the wasm build; [`decode`] wires it to libav
//! and the filesystem.

use std::io::{self, Write};

use anyhow::{anyhow, bail, Result};

use crate::constants::*;
use crate::frame::{decode_frame, frame_layout, read_blocks};
use crate::metadata::{parse_payload, sha256_hex, PayloadSink, QrMeta};
use crate::qr::decode_qr_metadata;
use crate::rs::rs_decode;

/// A block-aligned sliding window over the conceptual ECC byte stream. `bytes` (the
/// window's capacity) is always a multiple of `RS_BLOCK_SIZE`, so a window's contents
/// always line up with the same RS block boundaries a one-shot decode would use.
struct Window {
    start: usize,
    bytes: usize,
    buf: Vec<u8>,
    seen: Vec<bool>,
}

impl Window {
    fn new(bytes: usize) -> Self {
        Self {
            start: 0,
            bytes,
            buf: vec![0u8; bytes],
            seen: vec![false; bytes],
        }
    }

    /// Slide forward by one window's worth of bytes, reusing the existing buffers.
    fn advance(&mut self) {
        self.start += self.bytes;
        self.buf.fill(0);
        self.seen.fill(false);
    }
}

/// Write a decoded frame's packed bytes (`data`, at absolute ECC-stream offset
/// `abs_start`) into `window`, flushing + rotating it forward as many times as needed if
/// `data` extends past the window's current span. A no-op if `data` falls entirely before
/// the window's current start (stale/out-of-range).
fn write_into_window<W: Write>(
    window: &mut Window,
    payload: &mut PayloadSink<W>,
    total_ecc_len: usize,
    abs_start: usize,
    data: &[u8],
) -> Result<()> {
    let abs_end = abs_start + data.len();
    if abs_end <= window.start {
        return Ok(());
    }
    let mut pos = abs_start.max(window.start);
    while pos < abs_end {
        let window_end = window.start + window.bytes;
        if pos >= window_end {
            flush_window(window, payload, total_ecc_len)?;
            window.advance();
            continue;
        }
        let chunk_end = abs_end.min(window_end);
        let local = (pos - window.start)..(chunk_end - window.start);
        let src = (pos - abs_start)..(chunk_end - abs_start);
        window.buf[local.clone()].copy_from_slice(&data[src]);
        window.seen[local].fill(true);
        pos = chunk_end;
    }
    Ok(())
}

/// Reed-Solomon decode the still-live portion of `window` (erasures = byte positions not
/// yet marked `seen`, already window-relative — exactly what `rs_decode`'s per-block
/// erasure mapping expects) and stream the plaintext into `payload`. A no-op once the
/// window has advanced past `total_ecc_len`.
fn flush_window<W: Write>(
    window: &Window,
    payload: &mut PayloadSink<W>,
    total_ecc_len: usize,
) -> Result<()> {
    if window.start >= total_ecc_len {
        return Ok(());
    }
    let len = window.bytes.min(total_ecc_len - window.start);
    let erasures: Vec<usize> = window.seen[..len]
        .iter()
        .enumerate()
        .filter(|(_, &s)| !s)
        .map(|(i, _)| i)
        .collect();
    let er = (!erasures.is_empty()).then_some(erasures.as_slice());
    let decoded = rs_decode(&window.buf[..len], er)
        .map_err(|e| anyhow!("RS decode failed at ECC offset {}: {e}", window.start))?;
    payload.feed(&decoded)
}

enum Sink<W: Write> {
    Pending {
        buffered: Vec<(u64, Vec<u8>)>,
    },
    Resolved {
        window: Window,
        payload: PayloadSink<W>,
        dbpf: usize,
        frames: usize,
        /// One bit per data frame index: counts each frame once, however many times it's
        /// pushed (the browser decoder re-reads a few frames at each chunk seam).
        seen_frames: Vec<u64>,
        total_ecc_len: usize,
        sha_expected: String,
        filename: String,
    },
}

/// Set bit `i`, returning whether it was previously clear.
fn mark_seen(seen: &mut [u64], i: usize) -> bool {
    let (word, bit) = (i / 64, 1u64 << (i % 64));
    let fresh = seen[word] & bit == 0;
    seen[word] |= bit;
    fresh
}

/// Frame-level counters from a decode run.
#[derive(Debug, Clone, Copy, Default)]
pub struct DecodeStats {
    /// Frames pushed into the decoder.
    pub total: usize,
    /// Distinct data frames with a valid sync pattern and an in-range index.
    pub valid: usize,
    /// Frames whose sync pattern didn't match (QR frames always land here).
    pub sync_fail: usize,
    /// Data frames the QR metadata says the video carries (or, on the fallback path, the
    /// highest index seen + 1).
    pub expected_frames: usize,
    /// `expected_frames` minus valid ones, recovered (or not) by Reed-Solomon.
    pub missing: usize,
}

/// The result of [`FrameDecoder::finish`]: the recovered file now sits in `writer`.
/// A SHA-256 mismatch is reported via `sha_ok`, not an error, so callers can keep the
/// corrupted output for inspection.
pub struct DecodeOutcome<W> {
    pub filename: String,
    pub writer: W,
    pub size: u64,
    pub sha256: String,
    pub sha_expected: String,
    pub sha_ok: bool,
    pub stats: DecodeStats,
}

/// I/O-free streaming decoder. Push 1920x1080 rgb24 frames in stream order with
/// [`push_frame`](Self::push_frame), then call [`finish`](Self::finish).
///
/// `open(filename, size)` creates the output writer, called once — as soon as the QR
/// metadata names the file (or, on the fallback path, at finish).
pub struct FrameDecoder<W: Write, F: FnMut(&str, u64) -> io::Result<W>> {
    open: F,
    window_bytes: usize,
    index_bits: usize,
    header_bits: usize,
    data_bits_per_frame: usize,
    qr_meta: Option<QrMeta>,
    sink: Sink<W>,
    total: usize,
    sync_fail: usize,
    valid: usize,
}

impl<W: Write, F: FnMut(&str, u64) -> io::Result<W>> FrameDecoder<W, F> {
    /// `window_blocks` is the sliding RS window size (see `budget::blocks_for_budget`).
    pub fn new(window_blocks: usize, open: F) -> Self {
        let index_bits = DEFAULT_FRAME_INDEX_BITS;
        let (header_bits, data_bits_per_frame) = frame_layout(index_bits);
        Self {
            open,
            window_bytes: window_blocks.max(1) * RS_BLOCK_SIZE,
            index_bits,
            header_bits,
            data_bits_per_frame,
            qr_meta: None,
            sink: Sink::Pending {
                buffered: Vec::new(),
            },
            total: 0,
            sync_fail: 0,
            valid: 0,
        }
    }

    /// QR metadata, once a metadata frame has been read.
    pub fn qr_meta(&self) -> Option<&QrMeta> {
        self.qr_meta.as_ref()
    }

    /// Frames pushed so far.
    pub fn frames_pushed(&self) -> usize {
        self.total
    }

    /// Feed the next decoded frame (tightly-packed 1920x1080 rgb24).
    pub fn push_frame(&mut self, frame: &[u8]) -> Result<()> {
        if frame.len() != FRAME_WIDTH * FRAME_HEIGHT * 3 {
            bail!(
                "expected a {FRAME_WIDTH}x{FRAME_HEIGHT} rgb24 frame ({} bytes), got {} bytes",
                FRAME_WIDTH * FRAME_HEIGHT * 3,
                frame.len()
            );
        }
        let frame_number = self.total + 1; // 1-based, matches the encoder's QR span
        if frame_number <= METADATA_FRAMES && self.qr_meta.is_none() {
            if let Some(m) = decode_qr_metadata(frame) {
                self.adopt_qr_meta(m)?;
            }
        }
        let bits = read_blocks(frame);
        match decode_frame(&bits, self.index_bits, self.header_bits) {
            Some((idx, data)) => match &mut self.sink {
                Sink::Resolved {
                    window,
                    payload,
                    dbpf,
                    frames,
                    seen_frames,
                    total_ecc_len,
                    ..
                } => {
                    let i = idx as usize;
                    if i < *frames {
                        write_into_window(window, payload, *total_ecc_len, i * *dbpf, &data)?;
                        if mark_seen(seen_frames, i) {
                            self.valid += 1;
                        }
                    }
                }
                Sink::Pending { buffered } => buffered.push((idx, data)),
            },
            None => self.sync_fail += 1,
        }
        self.total += 1;
        Ok(())
    }

    fn adopt_qr_meta(&mut self, m: QrMeta) -> Result<()> {
        // Adopt the QR's index width if it differs from our assumption; any frames
        // buffered under the old width are unusable (mirrors the old `frames.clear()`).
        if let Some(ib) = m.index_bits {
            let ib = ib as usize;
            if (ib == DEFAULT_FRAME_INDEX_BITS || ib == EXTENDED_FRAME_INDEX_BITS)
                && ib != self.index_bits
            {
                self.index_bits = ib;
                let (h, d) = frame_layout(ib);
                self.header_bits = h;
                self.data_bits_per_frame = d;
                self.sink = Sink::Pending {
                    buffered: Vec::new(),
                };
            }
        }
        // Resolve the sink now that filename/size/sha256/frame-count are all known (the
        // first three are guaranteed together by `parse_qr_meta`'s gate; `frames` is the
        // one field it doesn't cover).
        if let (Some(frames), Some(filename), Some(size), Some(sha256)) =
            (m.frames, m.filename.clone(), m.size, m.sha256.clone())
        {
            let frames = frames as usize;
            let dbpf = self.data_bits_per_frame / 8;
            let raw_total = frames * dbpf;
            let total_ecc_len = match m.ecc_bytes.map(|e| e as usize) {
                Some(e) => e.min(raw_total),
                None if raw_total >= RS_BLOCK_SIZE => (raw_total / RS_BLOCK_SIZE) * RS_BLOCK_SIZE,
                None => raw_total,
            };
            let mut window = Window::new(self.window_bytes);
            let mut payload = PayloadSink::new((self.open)(&filename, size)?, size);
            let mut seen_frames = vec![0u64; frames.div_ceil(64)];
            if let Sink::Pending { buffered } = &self.sink {
                for (idx, data) in buffered {
                    let i = *idx as usize;
                    if i < frames {
                        write_into_window(
                            &mut window,
                            &mut payload,
                            total_ecc_len,
                            i * dbpf,
                            data,
                        )?;
                        if mark_seen(&mut seen_frames, i) {
                            self.valid += 1;
                        }
                    }
                }
            }
            self.sink = Sink::Resolved {
                window,
                payload,
                dbpf,
                frames,
                seen_frames,
                total_ecc_len,
                sha_expected: sha256,
                filename,
            };
        }
        self.qr_meta = Some(m);
        Ok(())
    }

    /// Flush the final window (or run the fallback decode) and return the outcome.
    pub fn finish(mut self) -> Result<DecodeOutcome<W>> {
        const NO_FRAMES: &str =
            "No valid frames found. Ensure the video is 1080p and FrameVault-encoded.";
        let sink = std::mem::replace(
            &mut self.sink,
            Sink::Pending {
                buffered: Vec::new(),
            },
        );
        match sink {
            Sink::Resolved {
                window,
                mut payload,
                total_ecc_len,
                sha_expected,
                filename,
                frames,
                ..
            } => {
                if self.valid == 0 {
                    bail!(NO_FRAMES);
                }
                flush_window(&window, &mut payload, total_ecc_len)?;
                let finished = payload.finish()?;
                let sha_ok = finished.sha256 == sha_expected;
                Ok(DecodeOutcome {
                    filename,
                    writer: finished.writer,
                    size: finished.bytes_written,
                    sha256: finished.sha256,
                    sha_expected,
                    sha_ok,
                    stats: DecodeStats {
                        total: self.total,
                        valid: self.valid,
                        sync_fail: self.sync_fail,
                        expected_frames: frames,
                        missing: frames.saturating_sub(self.valid),
                    },
                })
            }
            Sink::Pending { buffered } => self.finish_fallback(buffered),
        }
    }

    /// QR metadata never resolved a frame count: decode everything at end-of-stream.
    /// Sized by the observed frame count rather than the memory budget, but only ever
    /// reached when the QR channel itself failed or was malformed.
    fn finish_fallback(mut self, buffered: Vec<(u64, Vec<u8>)>) -> Result<DecodeOutcome<W>> {
        const NO_FRAMES: &str =
            "No valid frames found. Ensure the video is 1080p and FrameVault-encoded.";
        if buffered.is_empty() {
            bail!(NO_FRAMES);
        }
        let dbpf = self.data_bits_per_frame / 8;
        let frame_count = buffered.iter().map(|(idx, _)| *idx as usize).max().unwrap() + 1;
        let mut ecc_from_video = vec![0u8; frame_count * dbpf];
        let mut seen = vec![false; frame_count];
        for (idx, data) in &buffered {
            let i = *idx as usize;
            ecc_from_video[i * dbpf..(i + 1) * dbpf].copy_from_slice(data);
            seen[i] = true;
        }
        let valid = seen.iter().filter(|&&s| s).count();
        if valid == 0 {
            bail!(NO_FRAMES);
        }
        if ecc_from_video.len() >= RS_BLOCK_SIZE {
            ecc_from_video.truncate((ecc_from_video.len() / RS_BLOCK_SIZE) * RS_BLOCK_SIZE);
        }
        let erasures: Vec<usize> = seen
            .iter()
            .enumerate()
            .filter(|(_, &s)| !s)
            .flat_map(|(i, _)| (0..dbpf).map(move |j| i * dbpf + j))
            .filter(|&p| p < ecc_from_video.len())
            .collect();
        let er = (!erasures.is_empty()).then_some(erasures.as_slice());
        let payload_bytes =
            rs_decode(&ecc_from_video, er).map_err(|e| anyhow!("RS decode failed: {e}"))?;

        let qr = self.qr_meta.take().unwrap_or_default();
        let (meta, file_data) = parse_payload(&payload_bytes, qr.size)?;
        let sha_expected = qr.sha256.unwrap_or_else(|| meta.sha256.clone());
        let filename = qr.filename.unwrap_or_else(|| meta.filename.clone());

        let size = file_data.len() as u64;
        let mut payload = PayloadSink::new((self.open)(&filename, size)?, size);
        payload.feed_file_bytes(&file_data)?;
        let finished = payload.finish()?;
        debug_assert_eq!(finished.sha256, sha256_hex(&file_data));
        let sha_ok = finished.sha256 == sha_expected;
        Ok(DecodeOutcome {
            filename,
            writer: finished.writer,
            size: finished.bytes_written,
            sha256: finished.sha256,
            sha_expected,
            sha_ok,
            stats: DecodeStats {
                total: self.total,
                valid,
                sync_fail: self.sync_fail,
                expected_frames: frame_count,
                missing: frame_count - valid,
            },
        })
    }
}

#[cfg(feature = "native")]
pub use native::{decode, DecodeReport, Strategy};

#[cfg(feature = "native")]
mod native {
    use std::path::{Path, PathBuf};

    use anyhow::{bail, Result};

    use super::FrameDecoder;
    use crate::budget;
    use crate::media;
    use crate::metadata::TempOutput;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Strategy {
        VideoOnly,
    }

    #[derive(Debug, Clone)]
    pub struct DecodeReport {
        pub filename: String,
        pub size: usize,
        pub sha256: String,
        pub sha_ok: bool,
        pub strategy: Strategy,
        pub output_path: PathBuf,
    }

    pub fn decode(
        video_path: &Path,
        output_dir: &Path,
        memory: Option<&str>,
    ) -> Result<DecodeReport> {
        println!("Input: {}", video_path.display());

        let budget_bytes = budget::resolve_budget_bytes(memory)?;
        let window_blocks = budget::blocks_for_budget(budget_bytes);
        println!(
            "Memory budget: {} MiB ({window_blocks} RS blocks/window)",
            budget_bytes / (1024 * 1024)
        );

        println!("\n[1/3] Decoding video channel...");
        let mut decoder = FrameDecoder::new(window_blocks, |name: &str, _size: u64| {
            TempOutput::create(output_dir, name)
        });
        media::decode_video_frames(video_path, |frame| decoder.push_frame(frame))?;

        let outcome = decoder.finish()?;
        let s = outcome.stats;
        println!(
            "  Total: {} | Valid: {} | Sync failures: {}",
            s.total, s.valid, s.sync_fail
        );

        println!("\n[2/3] Reed-Solomon decode...");
        if s.missing == 0 {
            println!("  All {} frames present.", s.expected_frames);
        } else {
            println!("  Missing {} frames (recovered by RS).", s.missing);
        }
        println!("  RS decode succeeded on video stream.");

        println!("\n[3/3] Extracting file...");
        let out_path = outcome.writer.persist()?;
        println!("  Filename:  {}", outcome.filename);
        println!("  Size:      {} bytes", outcome.size);
        println!("  SHA256:    {}", outcome.sha256);
        println!(
            "  Integrity: {}",
            if outcome.sha_ok {
                "PASS"
            } else {
                "FAIL - data is corrupted"
            }
        );
        println!("  Saved to:  {}", out_path.display());

        if !outcome.sha_ok {
            bail!("SHA256 mismatch: the recovered file does not match the original.");
        }
        Ok(DecodeReport {
            filename: outcome.filename,
            size: outcome.size as usize,
            sha256: outcome.sha256,
            sha_ok: outcome.sha_ok,
            strategy: Strategy::VideoOnly,
            output_path: out_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::FrameEncoder;
    use crate::metadata::{assemble_payload, build_meta_from_reader, Meta};
    use crate::rs::ecc_encode;

    /// Drive the window/flush machinery over `ecc` in chunks sized by cycling through
    /// `chunk_sizes`, then finish and return the recovered file bytes.
    fn drive_window(
        ecc: &[u8],
        window_bytes: usize,
        chunk_sizes: &[usize],
        expected_size: u64,
    ) -> Vec<u8> {
        let mut window = Window::new(window_bytes);
        let mut payload = PayloadSink::new(Vec::new(), expected_size);
        let total_ecc_len = ecc.len();
        let mut pos = 0;
        let mut ci = 0;
        while pos < ecc.len() {
            let sz = chunk_sizes[ci % chunk_sizes.len()].max(1);
            let end = (pos + sz).min(ecc.len());
            write_into_window(
                &mut window,
                &mut payload,
                total_ecc_len,
                pos,
                &ecc[pos..end],
            )
            .unwrap();
            pos = end;
            ci += 1;
        }
        flush_window(&window, &mut payload, total_ecc_len).unwrap();
        payload.finish().unwrap().writer
    }

    #[test]
    fn window_streams_payload_byte_for_byte() {
        let file_bytes: Vec<u8> = (0..2000u32).map(|i| (i % 256) as u8).collect();
        let meta = Meta {
            v: ENCODING_VERSION,
            filename: "f.bin".to_string(),
            size: file_bytes.len() as u64,
            sha256: sha256_hex(&file_bytes),
        };
        let payload = assemble_payload(&meta, &file_bytes);
        let ecc = ecc_encode(&payload);

        for &window_blocks in &[1usize, 2, 5] {
            let window_bytes = window_blocks * RS_BLOCK_SIZE;
            let chunk_size_sets: &[&[usize]] = &[&[7, 13, 50], &[window_bytes + 3], &[1]];
            for &chunk_sizes in chunk_size_sets {
                let out = drive_window(&ecc, window_bytes, chunk_sizes, meta.size);
                assert_eq!(
                    out, file_bytes,
                    "window_blocks={window_blocks}, chunk_sizes={chunk_sizes:?}"
                );
            }
        }
    }

    /// Render a luma plane as the rgb24 frame a decoder would see (neutral chroma, full
    /// range): R = G = B = the BT.601 limited->full expansion of Y.
    fn luma_to_rgb(luma: &[u8]) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(luma.len() * 3);
        for &y in luma {
            let v = (((y as f32) - 16.0) * 255.0 / 219.0)
                .round()
                .clamp(0.0, 255.0) as u8;
            rgb.extend_from_slice(&[v, v, v]);
        }
        rgb
    }

    /// The full I/O-free path the wasm build uses: FrameEncoder -> frames -> FrameDecoder,
    /// with duplicated and dropped frames thrown in (like the browser's chunk overlap).
    #[test]
    fn frame_encoder_to_frame_decoder_round_trip() {
        let file: Vec<u8> = (0..5000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
            .collect();
        let meta = build_meta_from_reader("core.bin", file.len() as u64, &file[..]).unwrap();
        let mut enc = FrameEncoder::new(&meta, &file[..], 3).unwrap();
        let total = enc.plan().total_frames;

        let mut frames = Vec::new();
        while let Some(luma) = enc.next_luma().unwrap() {
            frames.push(luma_to_rgb(&luma));
        }
        assert_eq!(frames.len(), total);

        let mut opened = None;
        let mut dec = FrameDecoder::new(2, |name: &str, size| {
            opened = Some((name.to_string(), size));
            Ok(Vec::new())
        });
        for (i, f) in frames.iter().enumerate() {
            // Data frame 4 spans ECC bytes 228..285, straddling the RS block boundary at 255
            // (27 + 30 erasures, each <= 32), so dropping it is recoverable.
            if i == METADATA_FRAMES + 4 {
                continue; // drop a data frame: RS erasure
            }
            dec.push_frame(f).unwrap();
            if i == METADATA_FRAMES + 9 {
                dec.push_frame(f).unwrap(); // duplicate: harmless
            }
        }
        let out = dec.finish().unwrap();
        assert!(out.sha_ok);
        assert_eq!(out.filename, "core.bin");
        // The duplicate counts once; the dropped frame shows up as missing.
        assert_eq!(out.stats.valid, out.stats.expected_frames - 1);
        assert_eq!(out.stats.missing, 1);
        assert_eq!(out.writer, file);
        assert_eq!(opened, Some(("core.bin".to_string(), file.len() as u64)));
    }
}
