//! Encode pipeline: file -> payload -> RS ECC -> QR + video frames + audio FSK ->
//! libav MP4. Mirrors `encode()` in encode.py.

use std::path::Path;

use anyhow::{bail, Result};

use crate::audio::{make_audio_pcm, select_audio_bytes};
use crate::constants::*;
use crate::frame::{frame_layout, frames_needed, make_frame_luma};
use crate::media;
use crate::metadata::{build_payload, build_qr_metadata};
use crate::qr::make_qr_luma;
use crate::rs::ecc_encode;

/// Summary of an encode run (also drives test assertions).
#[derive(Debug, Clone)]
pub struct EncodeReport {
    pub filename: String,
    pub size: u64,
    pub sha256: String,
    pub ecc_len: usize,
    pub index_bits: usize,
    pub num_data_frames: usize,
    pub total_frames: usize,
    pub duration_sec: f64,
    pub audio_byte_count: usize,
    pub audio_layout: String,
}

pub fn encode(input_path: &Path, output_path: &Path) -> Result<EncodeReport> {
    println!("[1/5] Reading file: {}", input_path.display());
    let (payload, meta) = build_payload(input_path)?;
    println!("      {} | {} bytes", meta.filename, meta.size);
    println!("      SHA256: {}", meta.sha256);

    println!("\n[2/5] Reed-Solomon ECC...");
    let ecc_data = ecc_encode(&payload);
    println!("      ECC stream: {} bytes", ecc_data.len());

    // Frame-index width: 16-bit until the data needs more than 65,536 frames.
    let total_bits = ecc_data.len() * 8;
    let (_, data_bits_default) = frame_layout(DEFAULT_FRAME_INDEX_BITS);
    let frames_16 = frames_needed(total_bits, data_bits_default);
    let (index_bits, data_bits_per_frame) = if frames_16 as u64 <= MAX_FRAME_COUNT_DEFAULT {
        (DEFAULT_FRAME_INDEX_BITS, data_bits_default)
    } else {
        let (_, dbpf) = frame_layout(EXTENDED_FRAME_INDEX_BITS);
        let num = frames_needed(total_bits, dbpf);
        if num as u64 > MAX_FRAME_COUNT_EXTENDED {
            bail!("payload needs {num} frames, exceeding 32-bit frame index capacity");
        }
        (EXTENDED_FRAME_INDEX_BITS, dbpf)
    };

    // Unpack ECC bytes to bits (MSB first) and pad to a whole number of frames.
    let mut all_bits: Vec<u8> = Vec::with_capacity(ecc_data.len() * 8);
    for &b in &ecc_data {
        for k in (0..8).rev() {
            all_bits.push((b >> k) & 1);
        }
    }
    let rem = all_bits.len() % data_bits_per_frame;
    if rem != 0 {
        all_bits.resize(all_bits.len() + (data_bits_per_frame - rem), 0);
    }
    let num_data_frames = all_bits.len() / data_bits_per_frame;
    let total_frames = num_data_frames + METADATA_FRAMES;
    let duration_sec = total_frames as f64 / FRAME_RATE as f64;

    let audio_byte_count = ecc_data
        .len()
        .min((duration_sec * BYTES_PER_SEC_AUDIO as f64) as usize);
    let audio_payload = select_audio_bytes(&ecc_data, audio_byte_count, Some(AUDIO_LAYOUT));
    let qr_payload = build_qr_metadata(
        &meta,
        ecc_data.len(),
        num_data_frames as u64,
        index_bits,
        audio_byte_count,
        AUDIO_LAYOUT,
    );
    let qr_luma = make_qr_luma(&qr_payload)?;
    let pcm = make_audio_pcm(&audio_payload, duration_sec);

    let coverage = if ecc_data.is_empty() {
        0.0
    } else {
        audio_byte_count as f64 / ecc_data.len() as f64 * 100.0
    };
    println!("\n[3/5] Plan");
    println!("      Data frames:    {num_data_frames}");
    println!("      Total frames:   {total_frames} @ {FRAME_RATE}fps ({duration_sec:.1}s)");
    println!("      Frame index:    {index_bits} bits");
    println!("      Audio layout:   {AUDIO_LAYOUT}");
    println!(
        "      Audio covers:   {audio_byte_count}/{} bytes ({coverage:.1}%)",
        ecc_data.len()
    );

    println!("\n[4/5] Generating frames + audio, muxing via libav...");
    let report = EncodeReport {
        filename: meta.filename.clone(),
        size: meta.size,
        sha256: meta.sha256.clone(),
        ecc_len: ecc_data.len(),
        index_bits,
        num_data_frames,
        total_frames,
        duration_sec,
        audio_byte_count,
        audio_layout: AUDIO_LAYOUT.to_string(),
    };

    let bits = all_bits;
    let qrf = qr_luma;
    let frame_source = move |idx: usize| -> Vec<u8> {
        if idx < METADATA_FRAMES {
            qrf.clone()
        } else {
            let i = idx - METADATA_FRAMES;
            make_frame_luma(
                i as u64,
                &bits[i * data_bits_per_frame..(i + 1) * data_bits_per_frame],
                index_bits,
            )
        }
    };
    media::encode_to_file(output_path, total_frames, frame_source, &pcm)?;

    println!("\n[5/5] Done. Output: {}", output_path.display());
    Ok(report)
}
