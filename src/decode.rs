//! Decode pipeline: libav MP4 -> video block channel -> Reed-Solomon -> original file.
//!
//! Frames stream in one at a time. Once the QR metadata resolves the frame count and
//! index width, each decoded frame's packed bytes are written straight into a single
//! preallocated ECC buffer (tracked by a `seen` bitset) — no per-frame heap map and no
//! bit-per-byte intermediate. Missing frames become RS erasures.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::constants::*;
use crate::frame::{decode_frame, frame_layout, read_blocks};
use crate::media;
use crate::metadata::{parse_payload, sha256_hex, QrMeta};
use crate::qr::decode_qr_metadata;
use crate::rs::rs_decode;

/// Which RS strategy recovered the payload. With the audio side-channel removed, only
/// the video block channel remains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    VideoOnly,
}

/// Summary of a decode run (also drives test assertions).
#[derive(Debug, Clone)]
pub struct DecodeReport {
    pub filename: String,
    pub size: usize,
    pub sha256: String,
    pub sha_ok: bool,
    pub strategy: Strategy,
    pub output_path: PathBuf,
}

/// Where decoded data frames land. Until the QR metadata resolves the layout, frames are
/// buffered (in practice none — QR precedes every data frame); once resolved, frames are
/// written directly into the preallocated ECC buffer.
enum Sink {
    Pending { buffered: Vec<(u64, Vec<u8>)> },
    Resolved { ecc: Vec<u8>, seen: Vec<bool>, dbpf: usize, frames: usize },
}

pub fn decode(video_path: &Path, output_dir: &Path) -> Result<DecodeReport> {
    println!("Input: {}", video_path.display());

    // ---- [1/3] video channel ----
    println!("\n[1/3] Decoding video channel...");
    let mut index_bits = DEFAULT_FRAME_INDEX_BITS;
    let (mut header_bits, mut data_bits_per_frame) = frame_layout(index_bits);
    let mut qr_meta: Option<QrMeta> = None;
    let mut sink = Sink::Pending { buffered: Vec::new() };
    let mut total = 0usize;
    let mut sync_fail = 0usize;

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
                // Resolve the sink now that the frame count is known.
                if let Some(frames) = m.frames.map(|n| n as usize) {
                    let dbpf = data_bits_per_frame / 8;
                    let mut ecc = vec![0u8; frames * dbpf];
                    let mut seen = vec![false; frames];
                    if let Sink::Pending { buffered } = &sink {
                        for (idx, data) in buffered {
                            let i = *idx as usize;
                            if i < frames {
                                ecc[i * dbpf..(i + 1) * dbpf].copy_from_slice(data);
                                seen[i] = true;
                            }
                        }
                    }
                    sink = Sink::Resolved { ecc, seen, dbpf, frames };
                }
                qr_meta = Some(m);
            }
        }
        let bits = read_blocks(frame);
        match decode_frame(&bits, index_bits, header_bits) {
            Some((idx, data)) => match &mut sink {
                Sink::Resolved { ecc, seen, dbpf, frames } => {
                    let i = idx as usize;
                    if i < *frames {
                        ecc[i * *dbpf..(i + 1) * *dbpf].copy_from_slice(&data);
                        seen[i] = true;
                    }
                }
                Sink::Pending { buffered } => buffered.push((idx, data)),
            },
            None => sync_fail += 1,
        }
        total += 1;
    })?;

    // Resolve from the observed frames if the QR never gave us a frame count.
    let (mut ecc_from_video, seen, data_bytes_per_frame, frame_count) = match sink {
        Sink::Resolved { ecc, seen, dbpf, frames } => (ecc, seen, dbpf, frames),
        Sink::Pending { buffered } => {
            if buffered.is_empty() {
                bail!("No valid frames found. Ensure the video is 1080p and FrameVault-encoded.");
            }
            let dbpf = data_bits_per_frame / 8;
            let frames = buffered.iter().map(|(idx, _)| *idx as usize).max().unwrap() + 1;
            let mut ecc = vec![0u8; frames * dbpf];
            let mut seen = vec![false; frames];
            for (idx, data) in &buffered {
                let i = *idx as usize;
                if i < frames {
                    ecc[i * dbpf..(i + 1) * dbpf].copy_from_slice(data);
                    seen[i] = true;
                }
            }
            (ecc, seen, dbpf, frames)
        }
    };

    let valid = seen.iter().filter(|&&s| s).count();
    if valid == 0 {
        bail!("No valid frames found. Ensure the video is 1080p and FrameVault-encoded.");
    }
    println!("  Total: {total} | Valid: {valid} | Sync failures: {sync_fail}");

    // ---- [2/3] reassemble + truncate to the planned ECC length ----
    println!("\n[2/3] Reassembling frame data...");
    let expected_ecc_bytes = qr_meta.as_ref().and_then(|m| m.ecc_bytes).map(|n| n as usize);
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

    if let Some(e) = expected_ecc_bytes {
        if e <= ecc_from_video.len() {
            ecc_from_video.truncate(e);
        }
    } else if ecc_from_video.len() >= RS_BLOCK_SIZE {
        ecc_from_video.truncate((ecc_from_video.len() / RS_BLOCK_SIZE) * RS_BLOCK_SIZE);
    }
    println!("  Video ECC stream: {} bytes", ecc_from_video.len());

    // ---- [3/3] Reed-Solomon decode (video only) ----
    println!("\n[3/3] Reed-Solomon decode...");
    let erasures: Vec<usize> = missing
        .iter()
        .flat_map(|&i| (0..data_bytes_per_frame).map(move |j| i * data_bytes_per_frame + j))
        .filter(|&p| p < ecc_from_video.len())
        .collect();
    let er = (!erasures.is_empty()).then_some(erasures.as_slice());
    let payload = match rs_decode(&ecc_from_video, er) {
        Ok(p) => {
            println!("  RS decode succeeded on video stream.");
            p
        }
        Err(e) => bail!("RS decode failed: {e}"),
    };

    // ---- extract + verify ----
    println!("\nExtracting file...");
    let expected_size = qr_meta.as_ref().and_then(|m| m.size);
    let expected_sha = qr_meta.as_ref().and_then(|m| m.sha256.clone());
    let expected_filename = qr_meta.as_ref().and_then(|m| m.filename.clone());
    let (meta, file_data) = parse_payload(&payload, expected_size)?;

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
