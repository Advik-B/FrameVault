//! Decode pipeline: libav MP4 -> video block channel + audio FSK channel ->
//! Reed-Solomon (video-only / merged / audio-only) -> original file. Mirrors
//! `decode()` in decode.py.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::audio::{decode_fsk, merge_audio_bytes};
use crate::constants::*;
use crate::frame::{bits_to_bytes, decode_frame, frame_layout, read_blocks};
use crate::media;
use crate::metadata::{parse_payload, sha256_hex, QrMeta};
use crate::qr::decode_qr_metadata;
use crate::rs::rs_decode;

/// Which RS strategy recovered the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    VideoOnly,
    Merged,
    AudioOnly,
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

pub fn decode(video_path: &Path, output_dir: &Path) -> Result<DecodeReport> {
    println!("Input: {}", video_path.display());

    // ---- [1/5] video channel ----
    println!("\n[1/5] Decoding video channel...");
    let mut index_bits = DEFAULT_FRAME_INDEX_BITS;
    let (mut header_bits, mut data_bits_per_frame) = frame_layout(index_bits);
    let mut frames: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut qr_meta: Option<QrMeta> = None;
    let mut total = 0usize;
    let mut sync_fail = 0usize;

    media::decode_video_frames(video_path, |frame| {
        let frame_number = total + 1; // 1-based, matches the encoder's QR span
        if frame_number <= METADATA_FRAMES && qr_meta.is_none() {
            if let Some(m) = decode_qr_metadata(frame) {
                if let Some(ib) = m.index_bits {
                    let ib = ib as usize;
                    if (ib == DEFAULT_FRAME_INDEX_BITS || ib == EXTENDED_FRAME_INDEX_BITS)
                        && ib != index_bits
                    {
                        index_bits = ib;
                        let (h, d) = frame_layout(index_bits);
                        header_bits = h;
                        data_bits_per_frame = d;
                        frames.clear(); // re-decode under the correct index width
                    }
                }
                qr_meta = Some(m);
            }
        }
        let bits = read_blocks(frame);
        match decode_frame(&bits, index_bits, header_bits) {
            Some((idx, data)) => {
                frames.insert(idx, data);
            }
            None => sync_fail += 1,
        }
        total += 1;
    })?;

    println!(
        "  Total: {total} | Valid: {} | Sync failures: {sync_fail}",
        frames.len()
    );
    if frames.is_empty() {
        bail!("No valid frames found. Ensure the video is 1080p and FrameVault-encoded.");
    }
    let data_bytes_per_frame = data_bits_per_frame / 8;

    // ---- [2/5] reassemble video bits ----
    println!("\n[2/5] Reassembling frame data...");
    let expected_frames = qr_meta.as_ref().and_then(|m| m.frames).map(|n| n as usize);
    let expected_ecc_bytes = qr_meta.as_ref().and_then(|m| m.ecc_bytes).map(|n| n as usize);

    let frame_count = if let Some(ef) = expected_frames {
        frames.retain(|&k, _| (k as usize) < ef);
        ef
    } else {
        *frames.keys().max().unwrap() as usize + 1
    };

    let mut all_bits = vec![0u8; frame_count * data_bits_per_frame];
    let mut missing: Vec<usize> = Vec::new();
    for i in 0..frame_count {
        match frames.get(&(i as u64)) {
            Some(d) => all_bits[i * data_bits_per_frame..(i + 1) * data_bits_per_frame]
                .copy_from_slice(d),
            None => missing.push(i),
        }
    }
    if missing.is_empty() {
        println!("  All {frame_count} frames present.");
    } else {
        println!("  Missing {} frames (RS will attempt recovery).", missing.len());
    }
    let mut ecc_from_video = bits_to_bytes(&all_bits);
    if let Some(e) = expected_ecc_bytes {
        if e <= ecc_from_video.len() {
            ecc_from_video.truncate(e);
        }
    }
    println!("  Video ECC stream: {} bytes", ecc_from_video.len());

    // ---- [3/5] audio channel ----
    println!("\n[3/5] Decoding audio channel (4-FSK)...");
    let samples = media::decode_audio_samples(video_path)?;
    let audio_bytes = decode_fsk(&samples);
    println!("  Recovered: {} bytes", audio_bytes.len());

    // ---- [4/5] Reed-Solomon: video-only -> merged -> audio-only ----
    println!("\n[4/5] Reed-Solomon decode...");
    let mut ecc_video = ecc_from_video;
    if expected_ecc_bytes.is_none() && ecc_video.len() >= RS_BLOCK_SIZE {
        ecc_video.truncate((ecc_video.len() / RS_BLOCK_SIZE) * RS_BLOCK_SIZE);
    }
    let erasures_video: Vec<usize> = missing
        .iter()
        .flat_map(|&i| (0..data_bytes_per_frame).map(move |j| i * data_bytes_per_frame + j))
        .filter(|&p| p < ecc_video.len())
        .collect();

    let mut payload: Option<Vec<u8>> = None;
    let mut strategy = Strategy::VideoOnly;

    // Strategy A: video only — best for clean local files.
    let er = (!erasures_video.is_empty()).then_some(erasures_video.as_slice());
    match rs_decode(&ecc_video, er) {
        Ok(p) => {
            println!("  RS decode succeeded on video stream.");
            payload = Some(p);
        }
        Err(e) => println!("  Video-only RS failed: {e}"),
    }

    // Strategy B: merge audio bytes at their planned ECC positions, then RS.
    if payload.is_none() && !audio_bytes.is_empty() {
        let layout = qr_meta.as_ref().and_then(|m| m.audio_layout.as_deref());
        let expected_audio = qr_meta
            .as_ref()
            .and_then(|m| m.audio_bytes)
            .map(|n| n as usize)
            .unwrap_or(audio_bytes.len());
        let (merged, recovered) = merge_audio_bytes(&ecc_video, &audio_bytes, expected_audio, layout);
        let recovered_set: HashSet<usize> = recovered.into_iter().collect();
        let erasures_merged: Vec<usize> = erasures_video
            .iter()
            .copied()
            .filter(|p| !recovered_set.contains(p))
            .collect();
        let em = (!erasures_merged.is_empty()).then_some(erasures_merged.as_slice());
        match rs_decode(&merged, em) {
            Ok(p) => {
                println!("  RS decode succeeded on merged stream.");
                payload = Some(p);
                strategy = Strategy::Merged;
            }
            Err(e) => println!("  Merged RS failed: {e}"),
        }
    }

    // Strategy C: audio only (useful only when audio covers the whole stream).
    if payload.is_none() && !audio_bytes.is_empty() {
        let trimmed = (audio_bytes.len() / RS_BLOCK_SIZE) * RS_BLOCK_SIZE;
        if trimmed > 0 {
            match rs_decode(&audio_bytes[..trimmed], None) {
                Ok(p) => {
                    println!("  RS decode succeeded on audio stream.");
                    payload = Some(p);
                    strategy = Strategy::AudioOnly;
                }
                Err(e) => println!("  Audio-only RS failed: {e}"),
            }
        }
    }

    let payload = match payload {
        Some(p) => p,
        None => bail!("All RS decode strategies failed."),
    };

    // ---- [5/5] extract + verify ----
    println!("\n[5/5] Extracting file...");
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
        strategy,
        output_path: out_path,
    };

    if !sha_ok {
        bail!("SHA256 mismatch: the recovered file does not match the original.");
    }
    Ok(report)
}
