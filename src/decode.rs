//! Decode pipeline: libav MP4 -> video block channel -> Reed-Solomon -> original file.
//!
//! Frames stream in one at a time. Once the QR metadata resolves the frame count, index
//! width, filename, size, and SHA-256, each decoded frame's packed bytes are written into
//! a block-aligned sliding [`Window`] over the conceptual ECC byte stream — sized by the
//! memory budget (`--memory`, or a fraction of system RAM), not the file size. A window is
//! Reed-Solomon decoded and its plaintext streamed straight to disk via `PayloadSink` as
//! soon as it fills, so neither the ECC stream nor the recovered file is ever held whole
//! in memory. Missing bytes become RS erasures, scoped to whichever window they fall in.
//!
//! If the QR metadata channel never resolves a frame count (rare: a non-FrameVault or
//! badly corrupted video), decode falls back to an end-of-stream path bounded by file
//! size rather than budget, since there's no frame count to size a window against.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};

use crate::budget;
use crate::constants::*;
use crate::frame::{decode_frame, frame_layout, read_blocks};
use crate::media;
use crate::metadata::{parse_payload, sha256_hex, PayloadSink, QrMeta};
use crate::qr::decode_qr_metadata;
use crate::rs::rs_decode;

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
fn write_into_window(
    window: &mut Window,
    payload: &mut PayloadSink,
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
fn flush_window(window: &Window, payload: &mut PayloadSink, total_ecc_len: usize) -> Result<()> {
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

enum Sink {
    Pending {
        buffered: Vec<(u64, Vec<u8>)>,
    },
    Resolved {
        window: Window,
        payload: PayloadSink,
        dbpf: usize,
        frames: usize,
        total_ecc_len: usize,
        sha_expected: String,
        filename: String,
    },
}

pub fn decode(video_path: &Path, output_dir: &Path, memory: Option<&str>) -> Result<DecodeReport> {
    println!("Input: {}", video_path.display());

    let budget_bytes = budget::resolve_budget_bytes(memory)?;
    let window_blocks = budget::blocks_for_budget(budget_bytes);
    let window_bytes = window_blocks * RS_BLOCK_SIZE;
    println!(
        "Memory budget: {} MiB ({window_blocks} RS blocks/window)",
        budget_bytes / (1024 * 1024)
    );

    // ---- [1/3] video channel ----
    println!("\n[1/3] Decoding video channel...");
    let mut index_bits = DEFAULT_FRAME_INDEX_BITS;
    let (mut header_bits, mut data_bits_per_frame) = frame_layout(index_bits);
    let mut qr_meta: Option<QrMeta> = None;
    let mut sink = Sink::Pending { buffered: Vec::new() };
    let mut total = 0usize;
    let mut sync_fail = 0usize;
    let mut valid = 0usize;

    media::decode_video_frames(video_path, |frame| {
        let frame_number = total + 1; // 1-based, matches the encoder's QR span
        if frame_number <= METADATA_FRAMES && qr_meta.is_none() {
            if let Some(m) = decode_qr_metadata(frame) {
                // Adopt the QR's index width if it differs from our assumption; any frames
                // buffered under the old width are unusable (mirrors the old `frames.clear()`).
                if let Some(ib) = m.index_bits {
                    let ib = ib as usize;
                    if (ib == DEFAULT_FRAME_INDEX_BITS || ib == EXTENDED_FRAME_INDEX_BITS)
                        && ib != index_bits
                    {
                        index_bits = ib;
                        let (h, d) = frame_layout(index_bits);
                        header_bits = h;
                        data_bits_per_frame = d;
                        sink = Sink::Pending { buffered: Vec::new() };
                    }
                }
                // Resolve the sink now that filename/size/sha256/frame-count are all known
                // (the first three are guaranteed together by `parse_qr_meta`'s gate; `frames`
                // is the one field it doesn't cover).
                if let (Some(frames), Some(filename), Some(size), Some(sha256)) =
                    (m.frames, m.filename.clone(), m.size, m.sha256.clone())
                {
                    let frames = frames as usize;
                    let dbpf = data_bits_per_frame / 8;
                    let raw_total = frames * dbpf;
                    let total_ecc_len = if let Some(e) = m.ecc_bytes.map(|e| e as usize) {
                        if e <= raw_total {
                            e
                        } else {
                            raw_total
                        }
                    } else if raw_total >= RS_BLOCK_SIZE {
                        (raw_total / RS_BLOCK_SIZE) * RS_BLOCK_SIZE
                    } else {
                        raw_total
                    };
                    let mut window = Window::new(window_bytes);
                    let mut payload = PayloadSink::new(output_dir, &filename, size)?;
                    if let Sink::Pending { buffered } = &sink {
                        for (idx, data) in buffered {
                            let i = *idx as usize;
                            if i < frames {
                                write_into_window(&mut window, &mut payload, total_ecc_len, i * dbpf, data)?;
                            }
                        }
                    }
                    sink = Sink::Resolved {
                        window,
                        payload,
                        dbpf,
                        frames,
                        total_ecc_len,
                        sha_expected: sha256,
                        filename,
                    };
                }
                qr_meta = Some(m);
            }
        }
        let bits = read_blocks(frame);
        match decode_frame(&bits, index_bits, header_bits) {
            Some((idx, data)) => match &mut sink {
                Sink::Resolved { window, payload, dbpf, frames, total_ecc_len, .. } => {
                    let i = idx as usize;
                    if i < *frames {
                        write_into_window(window, payload, *total_ecc_len, i * *dbpf, &data)?;
                        valid += 1;
                    }
                }
                Sink::Pending { buffered } => buffered.push((idx, data)),
            },
            None => sync_fail += 1,
        }
        total += 1;
        Ok(())
    })?;

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
            println!("  Total: {total} | Valid: {valid} | Sync failures: {sync_fail}");
            if valid == 0 {
                bail!("No valid frames found. Ensure the video is 1080p and FrameVault-encoded.");
            }

            println!("\n[2/3] Finalizing Reed-Solomon decode...");
            let missing = frames.saturating_sub(valid);
            if missing == 0 {
                println!("  All {frames} frames present.");
            } else {
                println!("  Missing {missing} frames (RS will attempt recovery).");
            }
            flush_window(&window, &mut payload, total_ecc_len)?;
            println!("  RS decode succeeded on video stream.");

            println!("\n[3/3] Extracting file...");
            let finished = payload.finish()?;
            let sha_ok = finished.sha256 == sha_expected;
            let sha256 = finished.sha256.clone();
            let bytes_written = finished.bytes_written;
            let out_path = finished.persist()?;

            println!("  Filename:  {filename}");
            println!("  Size:      {bytes_written} bytes");
            println!("  SHA256:    {sha256}");
            println!(
                "  Integrity: {}",
                if sha_ok { "PASS" } else { "FAIL - data is corrupted" }
            );
            println!("  Saved to:  {}", out_path.display());

            let report = DecodeReport {
                filename,
                size: bytes_written as usize,
                sha256,
                sha_ok,
                strategy: Strategy::VideoOnly,
                output_path: out_path,
            };

            if !sha_ok {
                bail!("SHA256 mismatch: the recovered file does not match the original.");
            }
            Ok(report)
        }
        Sink::Pending { buffered } => {
            // QR metadata never resolved a frame count: fall back to the old end-of-stream
            // path. Sized by the observed frame count rather than the memory budget, but
            // only ever reached when the QR channel itself failed or was malformed.
            if buffered.is_empty() {
                bail!("No valid frames found. Ensure the video is 1080p and FrameVault-encoded.");
            }
            let dbpf = data_bits_per_frame / 8;
            let frame_count = buffered.iter().map(|(idx, _)| *idx as usize).max().unwrap() + 1;
            let mut ecc_from_video = vec![0u8; frame_count * dbpf];
            let mut seen = vec![false; frame_count];
            for (idx, data) in &buffered {
                let i = *idx as usize;
                if i < frame_count {
                    ecc_from_video[i * dbpf..(i + 1) * dbpf].copy_from_slice(data);
                    seen[i] = true;
                }
            }

            let valid = seen.iter().filter(|&&s| s).count();
            if valid == 0 {
                bail!("No valid frames found. Ensure the video is 1080p and FrameVault-encoded.");
            }
            println!("  Total: {total} | Valid: {valid} | Sync failures: {sync_fail}");

            println!("\n[2/3] Reassembling frame data...");
            let missing: Vec<usize> = seen
                .iter()
                .enumerate()
                .filter(|(_, &s)| !s)
                .map(|(i, _)| i)
                .collect();
            if missing.is_empty() {
                println!("  All {frame_count} frames present.");
            } else {
                println!("  Missing {} frames (RS will attempt recovery).", missing.len());
            }
            if ecc_from_video.len() >= RS_BLOCK_SIZE {
                ecc_from_video.truncate((ecc_from_video.len() / RS_BLOCK_SIZE) * RS_BLOCK_SIZE);
            }
            println!("  Video ECC stream: {} bytes", ecc_from_video.len());

            println!("\n[3/3] Reed-Solomon decode...");
            let erasures: Vec<usize> = missing
                .iter()
                .flat_map(|&i| (0..dbpf).map(move |j| i * dbpf + j))
                .filter(|&p| p < ecc_from_video.len())
                .collect();
            let er = (!erasures.is_empty()).then_some(erasures.as_slice());
            let payload_bytes = match rs_decode(&ecc_from_video, er) {
                Ok(p) => {
                    println!("  RS decode succeeded on video stream.");
                    p
                }
                Err(e) => bail!("RS decode failed: {e}"),
            };

            println!("\nExtracting file...");
            let expected_size = qr_meta.as_ref().and_then(|m| m.size);
            let expected_sha = qr_meta.as_ref().and_then(|m| m.sha256.clone());
            let expected_filename = qr_meta.as_ref().and_then(|m| m.filename.clone());
            let (meta, file_data) = parse_payload(&payload_bytes, expected_size)?;

            let sha_computed = sha256_hex(&file_data);
            let sha_expected = expected_sha.unwrap_or_else(|| meta.sha256.clone());
            let sha_ok = sha_computed == sha_expected;

            let out_name = expected_filename.unwrap_or_else(|| meta.filename.clone());
            let out_path = output_dir.join(&out_name);
            std::fs::write(&out_path, &file_data)?;

            println!("  Filename:  {out_name}");
            println!("  Size:      {} bytes", file_data.len());
            println!("  SHA256:    {sha_computed}");
            println!(
                "  Integrity: {}",
                if sha_ok { "PASS" } else { "FAIL - data is corrupted" }
            );
            println!("  Saved to:  {}", out_path.display());

            let report = DecodeReport {
                filename: out_name,
                size: file_data.len(),
                sha256: sha_computed,
                sha_ok,
                strategy: Strategy::VideoOnly,
                output_path: out_path,
            };

            if !sha_ok {
                bail!("SHA256 mismatch: the recovered file does not match the original.");
            }
            Ok(report)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{assemble_payload, Meta};
    use crate::rs::ecc_encode;
    use std::path::Path;

    /// Drive the window/flush machinery over `ecc` in chunks sized by cycling through
    /// `chunk_sizes`, then finish + persist and return the recovered file bytes.
    fn drive_window(
        ecc: &[u8],
        window_bytes: usize,
        chunk_sizes: &[usize],
        out_dir: &Path,
        filename: &str,
        expected_size: u64,
    ) -> Vec<u8> {
        let mut window = Window::new(window_bytes);
        let mut payload = PayloadSink::new(out_dir, filename, expected_size).unwrap();
        let total_ecc_len = ecc.len();
        let mut pos = 0;
        let mut ci = 0;
        while pos < ecc.len() {
            let sz = chunk_sizes[ci % chunk_sizes.len()].max(1);
            let end = (pos + sz).min(ecc.len());
            write_into_window(&mut window, &mut payload, total_ecc_len, pos, &ecc[pos..end]).unwrap();
            pos = end;
            ci += 1;
        }
        flush_window(&window, &mut payload, total_ecc_len).unwrap();
        let finished = payload.finish().unwrap();
        let out_path = finished.persist().unwrap();
        std::fs::read(&out_path).unwrap()
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
                let dir = tempfile::tempdir().unwrap();
                let out = drive_window(
                    &ecc,
                    window_bytes,
                    chunk_sizes,
                    dir.path(),
                    &meta.filename,
                    meta.size,
                );
                assert_eq!(
                    out, file_bytes,
                    "window_blocks={window_blocks}, chunk_sizes={chunk_sizes:?}"
                );
            }
        }
    }
}
