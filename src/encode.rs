//! Encode pipeline: file -> `[meta header | raw bytes]` payload -> streaming RS ECC ->
//! QR + video frames -> libav MP4.
//!
//! Two passes over the file, so neither the file nor the ECC stream is ever held whole
//! in memory: pass 1 stream-hashes the file to build the metadata and plan the layout
//! (every size is a pure function of file size + filename); pass 2 chains the header and
//! file bytes through a [`StreamingEcc`] and renders frames on demand.

use std::fs::File;
use std::io::{BufReader, Cursor, Read};
use std::path::Path;

use anyhow::{bail, Result};

use crate::budget;
use crate::constants::*;
use crate::frame::{frame_layout, make_frame_luma, select_index_bits};
use crate::media;
use crate::metadata::{build_meta, build_qr_metadata, serialize_meta_header};
use crate::qr::make_qr_luma;
use crate::rs::ecc_len_for;
use crate::stream::StreamingEcc;

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
}

/// Pulls fixed-size frames of packed ECC bytes from a [`StreamingEcc`] and renders each
/// to a luma plane. The final short frame is zero-padded — matching the old encoder's
/// padding of the bit stream to a whole number of frames.
struct DataFrameProducer<R: Read> {
    ecc: StreamingEcc<R>,
    index_bits: usize,
    emitted: usize,
    buf: Vec<u8>,
}

impl<R: Read> DataFrameProducer<R> {
    fn new(ecc: StreamingEcc<R>, data_bytes_per_frame: usize, index_bits: usize) -> Self {
        Self {
            ecc,
            index_bits,
            emitted: 0,
            buf: vec![0u8; data_bytes_per_frame],
        }
    }

    fn next_frame(&mut self) -> Result<Vec<u8>> {
        let mut filled = 0;
        while filled < self.buf.len() {
            let n = self.ecc.read(&mut self.buf[filled..])?;
            if n == 0 {
                break; // end of ECC stream: the rest of this frame is zero padding
            }
            filled += n;
        }
        self.buf[filled..].fill(0);
        let luma = make_frame_luma(self.emitted as u64, &self.buf, self.index_bits);
        self.emitted += 1;
        Ok(luma)
    }
}

pub fn encode(input_path: &Path, output_path: &Path, memory: Option<&str>) -> Result<EncodeReport> {
    // ---- [1/4] pass 1: stream-hash the file + build metadata ----
    println!("[1/4] Reading file: {}", input_path.display());
    let meta = build_meta(input_path)?;
    println!("      {} | {} bytes", meta.filename, meta.size);
    println!("      SHA256: {}", meta.sha256);

    // ---- [2/4] plan the layout (pure function of size + filename) ----
    let header = serialize_meta_header(&meta);
    let payload_len = header.len() + meta.size as usize;
    let ecc_len = ecc_len_for(payload_len);

    let index_bits = select_index_bits(ecc_len);
    let (_, data_bits_per_frame) = frame_layout(index_bits);
    let data_bytes_per_frame = data_bits_per_frame / 8;
    let num_data_frames = ecc_len.div_ceil(data_bytes_per_frame);
    if num_data_frames as u64 > MAX_FRAME_COUNT_EXTENDED {
        bail!("payload needs {num_data_frames} frames, exceeding 32-bit frame index capacity");
    }
    let total_frames = num_data_frames + METADATA_FRAMES;
    let duration_sec = total_frames as f64 / FRAME_RATE as f64;

    let budget_bytes = budget::resolve_budget_bytes(memory)?;
    let batch_blocks = budget::blocks_for_budget(budget_bytes);

    println!("\n[2/4] Plan");
    println!("      ECC stream:     {ecc_len} bytes");
    println!("      Data frames:    {num_data_frames}");
    println!("      Total frames:   {total_frames} @ {FRAME_RATE}fps ({duration_sec:.1}s)");
    println!("      Frame index:    {index_bits} bits");
    println!(
        "      Memory budget:  {} MiB ({batch_blocks} RS blocks/batch)",
        budget_bytes / (1024 * 1024)
    );

    let qr_payload = build_qr_metadata(&meta, ecc_len, num_data_frames as u64, index_bits);
    let qr_luma = make_qr_luma(&qr_payload)?;

    let report = EncodeReport {
        filename: meta.filename.clone(),
        size: meta.size,
        sha256: meta.sha256.clone(),
        ecc_len,
        index_bits,
        num_data_frames,
        total_frames,
        duration_sec,
    };

    // ---- [3/4] pass 2: stream RS-encode + render frames, muxing via libav ----
    println!("\n[3/4] Generating frames, muxing via libav...");
    let payload_reader = Cursor::new(header).chain(BufReader::new(File::open(input_path)?));
    let mut producer = DataFrameProducer::new(
        StreamingEcc::new(payload_reader, batch_blocks),
        data_bytes_per_frame,
        index_bits,
    );
    let make_luma = move |idx: usize| -> Result<Vec<u8>> {
        if idx < METADATA_FRAMES {
            Ok(qr_luma.clone())
        } else {
            producer.next_frame()
        }
    };
    media::encode_to_file(output_path, total_frames, make_luma)?;

    println!("\n[4/4] Done. Output: {}", output_path.display());
    Ok(report)
}
