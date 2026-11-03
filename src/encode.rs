//! Encode pipeline: file -> `[meta header | raw bytes]` payload -> streaming RS ECC ->
//! QR + video frames -> MP4.
//!
//! Two passes over the input, so neither the file nor the ECC stream is ever held whole
//! in memory: pass 1 stream-hashes the input to build the metadata and plan the layout
//! (every size is a pure function of file size + filename); pass 2 chains the header and
//! file bytes through a [`StreamingEcc`] and renders frames on demand.
//!
//! [`FrameEncoder`] is the I/O-free core (pass 2 → luma planes) shared by the native CLI
//! and the wasm build; [`encode`] wires it to the filesystem and libav.

use std::io::{Chain, Cursor, Read};

use anyhow::{bail, Result};

use crate::constants::*;
use crate::frame::{frame_layout, make_frame_luma, select_index_bits};
use crate::metadata::{build_qr_metadata, serialize_meta_header, Meta};
use crate::qr::make_qr_luma;
use crate::rs::ecc_len_for;
use crate::stream::StreamingEcc;

/// Frame layout for one payload — a pure function of file size + filename.
#[derive(Debug, Clone)]
pub struct EncodePlan {
    pub ecc_len: usize,
    pub index_bits: usize,
    pub data_bytes_per_frame: usize,
    pub num_data_frames: usize,
    pub total_frames: usize,
    pub duration_sec: f64,
}

impl EncodePlan {
    pub fn for_meta(meta: &Meta) -> Result<Self> {
        let header_len = serialize_meta_header(meta).len();
        let ecc_len = ecc_len_for(header_len + meta.size as usize);
        let index_bits = select_index_bits(ecc_len);
        let (_, data_bits_per_frame) = frame_layout(index_bits);
        let data_bytes_per_frame = data_bits_per_frame / 8;
        let num_data_frames = ecc_len.div_ceil(data_bytes_per_frame);
        if num_data_frames as u64 > MAX_FRAME_COUNT_EXTENDED {
            bail!("payload needs {num_data_frames} frames, exceeding 32-bit frame index capacity");
        }
        let total_frames = num_data_frames + METADATA_FRAMES;
        Ok(Self {
            ecc_len,
            index_bits,
            data_bytes_per_frame,
            num_data_frames,
            total_frames,
            duration_sec: total_frames as f64 / FRAME_RATE as f64,
        })
    }
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

/// I/O-free encoder: yields the video's luma planes (1920x1080 Y bytes) in order —
/// `METADATA_FRAMES` QR frames, then the data frames. `file` must yield exactly
/// `meta.size` bytes (the same bytes `meta.sha256` was computed over).
pub struct FrameEncoder<R: Read> {
    plan: EncodePlan,
    qr_luma: Vec<u8>,
    producer: DataFrameProducer<Chain<Cursor<Vec<u8>>, R>>,
    next: usize,
}

impl<R: Read> FrameEncoder<R> {
    /// `batch_blocks` is the RS streaming batch (see `budget::blocks_for_budget`).
    pub fn new(meta: &Meta, file: R, batch_blocks: usize) -> Result<Self> {
        let plan = EncodePlan::for_meta(meta)?;
        let qr_payload = build_qr_metadata(
            meta,
            plan.ecc_len,
            plan.num_data_frames as u64,
            plan.index_bits,
        );
        let qr_luma = make_qr_luma(&qr_payload)?;
        let payload = Cursor::new(serialize_meta_header(meta)).chain(file);
        let producer = DataFrameProducer::new(
            StreamingEcc::new(payload, batch_blocks),
            plan.data_bytes_per_frame,
            plan.index_bits,
        );
        Ok(Self {
            plan,
            qr_luma,
            producer,
            next: 0,
        })
    }

    pub fn plan(&self) -> &EncodePlan {
        &self.plan
    }

    /// Frames emitted so far.
    pub fn position(&self) -> usize {
        self.next
    }

    /// The next luma plane, or `None` once all `plan().total_frames` have been emitted.
    pub fn next_luma(&mut self) -> Result<Option<Vec<u8>>> {
        if self.next >= self.plan.total_frames {
            return Ok(None);
        }
        let luma = if self.next < METADATA_FRAMES {
            self.qr_luma.clone()
        } else {
            self.producer.next_frame()?
        };
        self.next += 1;
        Ok(Some(luma))
    }
}

#[cfg(feature = "native")]
pub use native::{encode, EncodeReport};

#[cfg(feature = "native")]
mod native {
    use std::fs::File;
    use std::io::BufReader;
    use std::path::Path;

    use anyhow::{anyhow, Result};

    use super::FrameEncoder;
    use crate::budget;
    use crate::constants::FRAME_RATE;
    use crate::media::{self, VideoCodec};
    use crate::metadata::build_meta;

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

    pub fn encode(
        input_path: &Path,
        output_path: &Path,
        memory: Option<&str>,
        codec: VideoCodec,
    ) -> Result<EncodeReport> {
        // ---- [1/4] pass 1: stream-hash the file + build metadata ----
        println!("[1/4] Reading file: {}", input_path.display());
        let meta = build_meta(input_path)?;
        println!("      {} | {} bytes", meta.filename, meta.size);
        println!("      SHA256: {}", meta.sha256);

        // ---- [2/4] plan the layout (pure function of size + filename) ----
        let budget_bytes = budget::resolve_budget_bytes(memory)?;
        let batch_blocks = budget::blocks_for_budget(budget_bytes);
        let file = BufReader::new(File::open(input_path)?);
        let mut encoder = FrameEncoder::new(&meta, file, batch_blocks)?;
        let plan = encoder.plan().clone();

        println!("\n[2/4] Plan");
        println!("      ECC stream:     {} bytes", plan.ecc_len);
        println!("      Data frames:    {}", plan.num_data_frames);
        println!(
            "      Total frames:   {} @ {FRAME_RATE}fps ({:.1}s)",
            plan.total_frames, plan.duration_sec
        );
        println!("      Frame index:    {} bits", plan.index_bits);
        println!("      Codec:          {codec}");
        println!(
            "      Memory budget:  {} MiB ({batch_blocks} RS blocks/batch)",
            budget_bytes / (1024 * 1024)
        );

        // ---- [3/4] pass 2: stream RS-encode + render frames, muxing via libav ----
        println!("\n[3/4] Generating frames, muxing via libav...");
        media::encode_to_file(output_path, plan.total_frames, codec, |_| {
            encoder
                .next_luma()?
                .ok_or_else(|| anyhow!("frame encoder ended early"))
        })?;

        println!("\n[4/4] Done. Output: {}", output_path.display());
        Ok(EncodeReport {
            filename: meta.filename,
            size: meta.size,
            sha256: meta.sha256,
            ecc_len: plan.ecc_len,
            index_bits: plan.index_bits,
            num_data_frames: plan.num_data_frames,
            total_frames: plan.total_frames,
            duration_sec: plan.duration_sec,
        })
    }
}
