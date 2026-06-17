//! Payload framing and QR metadata. Mirrors the payload/QR helpers in
//! encode.py / decode.py.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::constants::{ENCODING_VERSION, METADATA_FRAMES};

/// Self-describing payload metadata header (stored before the raw file bytes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub v: u32,
    pub filename: String,
    pub size: u64,
    pub sha256: String,
}

/// Lowercase hex-encode a digest (or any byte sequence).
fn hex_digest(bytes: impl IntoIterator<Item = u8>) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Lowercase hex SHA-256 digest.
pub fn sha256_hex(data: &[u8]) -> String {
    hex_digest(Sha256::digest(data))
}

/// Build the [`Meta`] for a file by streaming it: size from the filesystem, SHA-256
/// by incremental hashing in fixed-size chunks. Never buffers the whole file, so it
/// stays bounded in memory regardless of file size.
pub fn build_meta(path: &Path) -> std::io::Result<Meta> {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file.bin")
        .to_string();
    let size = std::fs::metadata(path)?.len();

    let mut hasher = Sha256::new();
    let mut reader = BufReader::new(File::open(path)?);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let sha256 = hex_digest(hasher.finalize());

    Ok(Meta {
        v: ENCODING_VERSION,
        filename,
        size,
        sha256,
    })
}

/// Serialize just the payload header — `[4-byte BE meta length][meta JSON]` — without
/// the raw file bytes. The full payload is this header followed by the file's bytes,
/// which the streaming encoder chains together rather than materializing.
pub fn serialize_meta_header(meta: &Meta) -> Vec<u8> {
    let meta_bytes = serde_json::to_vec(meta).expect("serialize meta");
    let mut header = Vec::with_capacity(4 + meta_bytes.len());
    header.extend_from_slice(&(meta_bytes.len() as u32).to_be_bytes());
    header.extend_from_slice(&meta_bytes);
    header
}

/// Assemble a full payload from metadata and raw bytes: `[header][raw bytes]`.
pub fn assemble_payload(meta: &Meta, raw: &[u8]) -> Vec<u8> {
    let mut payload = serialize_meta_header(meta);
    payload.extend_from_slice(raw);
    payload
}

/// Parse a payload back into `(metadata, file bytes)`. `expected_size` overrides
/// the size declared in the metadata when known (e.g. from the QR header).
pub fn parse_payload(payload: &[u8], expected_size: Option<u64>) -> anyhow::Result<(Meta, Vec<u8>)> {
    if payload.len() < 4 {
        anyhow::bail!("payload too short for metadata length");
    }
    let meta_len = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    if payload.len() < 4 + meta_len {
        anyhow::bail!("payload too short for metadata");
    }
    let meta: Meta = serde_json::from_slice(&payload[4..4 + meta_len])?;
    let file_start = 4 + meta_len;
    let file_size = expected_size.unwrap_or(meta.size) as usize;
    let end = file_start
        .checked_add(file_size)
        .ok_or_else(|| anyhow::anyhow!("file size overflow"))?;
    if payload.len() < end {
        anyhow::bail!("payload ended before expected file size");
    }
    Ok((meta, payload[file_start..end].to_vec()))
}

/// Compact decoder metadata carried by the QR frames. Serializes with short keys
/// (`v`, `f`, `s`, ...) and also accepts the long aliases on deserialization.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QrMeta {
    #[serde(rename = "v", alias = "version", skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(rename = "f", alias = "filename", skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(rename = "s", alias = "size", skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(rename = "h", alias = "sha256", skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(rename = "e", alias = "ecc_bytes", skip_serializing_if = "Option::is_none")]
    pub ecc_bytes: Option<u64>,
    #[serde(rename = "n", alias = "frames", skip_serializing_if = "Option::is_none")]
    pub frames: Option<u64>,
    #[serde(rename = "i", alias = "index_bits", skip_serializing_if = "Option::is_none")]
    pub index_bits: Option<u32>,
    #[serde(rename = "m", alias = "metadata_frames", skip_serializing_if = "Option::is_none")]
    pub metadata_frames: Option<u64>,
}

/// Build the compact QR metadata JSON bytes for the decoder.
pub fn build_qr_metadata(meta: &Meta, ecc_len: usize, num_frames: u64, index_bits: usize) -> Vec<u8> {
    let qr = QrMeta {
        version: Some(ENCODING_VERSION),
        filename: Some(meta.filename.clone()),
        size: Some(meta.size),
        sha256: Some(meta.sha256.clone()),
        ecc_bytes: Some(ecc_len as u64),
        frames: Some(num_frames),
        index_bits: Some(index_bits as u32),
        metadata_frames: Some(METADATA_FRAMES as u64),
    };
    serde_json::to_vec(&qr).expect("serialize qr meta")
}

/// Parse QR metadata JSON. Returns `None` on a parse error or when the required
/// fields (filename, size, sha256) are missing — mirrors the Python validity check.
pub fn parse_qr_meta(json: &str) -> Option<QrMeta> {
    let meta: QrMeta = serde_json::from_str(json).ok()?;
    if meta.filename.is_some() && meta.size.is_some() && meta.sha256.is_some() {
        Some(meta)
    } else {
        None
    }
}

/// Tracks progress through the embedded payload header (`[4-byte BE meta length][meta
/// JSON]`) so [`PayloadSink::feed`] can skip it without parsing it. By the time decode
/// reaches the windowed path, QR metadata has already supplied filename/size/sha256 —
/// see `parse_qr_meta`'s required-fields gate above — so the header's own copy of that
/// information is redundant and only its length matters.
enum HeaderState {
    ReadingLenPrefix { have: Vec<u8> },
    SkippingMeta { remaining: usize },
    Streaming,
}

/// Streams RS-decoded plaintext straight to disk: skips the embedded payload header,
/// then writes the remaining file bytes to a temp file in `output_dir` while
/// incrementally SHA-256 hashing them. Never holds the recovered file whole in memory.
pub struct PayloadSink {
    state: HeaderState,
    writer: BufWriter<NamedTempFile>,
    hasher: Sha256,
    bytes_written: u64,
    remaining_file_bytes: u64,
    out_path: PathBuf,
}

impl PayloadSink {
    pub fn new(output_dir: &Path, filename: &str, expected_size: u64) -> io::Result<Self> {
        // Created inside `output_dir` (not the system temp dir) so `FinishedPayload::persist`'s
        // rename stays on the same filesystem.
        let tmp = NamedTempFile::new_in(output_dir)?;
        Ok(Self {
            state: HeaderState::ReadingLenPrefix { have: Vec::with_capacity(4) },
            writer: BufWriter::new(tmp),
            hasher: Sha256::new(),
            bytes_written: 0,
            remaining_file_bytes: expected_size,
            out_path: output_dir.join(filename),
        })
    }

    /// Feed the next chunk of RS-decoded plaintext, in stream order. A single chunk may
    /// straddle the len-prefix/meta/file boundaries arbitrarily, since window sizes don't
    /// align to header sizes. Bytes beyond `expected_size` (the final frame's zero-pad)
    /// are silently dropped.
    pub fn feed(&mut self, mut chunk: &[u8]) -> Result<()> {
        while !chunk.is_empty() {
            match std::mem::replace(&mut self.state, HeaderState::Streaming) {
                HeaderState::ReadingLenPrefix { mut have } => {
                    let take = (4 - have.len()).min(chunk.len());
                    have.extend_from_slice(&chunk[..take]);
                    chunk = &chunk[take..];
                    self.state = if have.len() == 4 {
                        let meta_len = u32::from_be_bytes([have[0], have[1], have[2], have[3]]);
                        HeaderState::SkippingMeta { remaining: meta_len as usize }
                    } else {
                        HeaderState::ReadingLenPrefix { have }
                    };
                }
                HeaderState::SkippingMeta { remaining } => {
                    let skip = remaining.min(chunk.len());
                    chunk = &chunk[skip..];
                    let remaining = remaining - skip;
                    self.state = if remaining == 0 {
                        HeaderState::Streaming
                    } else {
                        HeaderState::SkippingMeta { remaining }
                    };
                }
                HeaderState::Streaming => {
                    let take = (chunk.len() as u64).min(self.remaining_file_bytes) as usize;
                    if take == 0 {
                        self.state = HeaderState::Streaming;
                        break; // rest of `chunk` is trailing zero-pad past the file's end
                    }
                    self.writer.write_all(&chunk[..take])?;
                    self.hasher.update(&chunk[..take]);
                    self.bytes_written += take as u64;
                    self.remaining_file_bytes -= take as u64;
                    chunk = &chunk[take..];
                    self.state = HeaderState::Streaming;
                }
            }
        }
        Ok(())
    }

    /// Flush to disk; consumes self and returns the computed digest + a handle that can
    /// rename the temp file into place.
    pub fn finish(mut self) -> Result<FinishedPayload> {
        self.writer.flush()?;
        let tmp = self
            .writer
            .into_inner()
            .map_err(|e| anyhow!("flush recovered file: {e}"))?;
        Ok(FinishedPayload {
            sha256: hex_digest(self.hasher.finalize()),
            bytes_written: self.bytes_written,
            tmp,
            out_path: self.out_path,
        })
    }
}

/// A fully-written, hashed recovered file, still sitting in a temp file pending
/// [`FinishedPayload::persist`].
pub struct FinishedPayload {
    pub sha256: String,
    pub bytes_written: u64,
    tmp: NamedTempFile,
    out_path: PathBuf,
}

impl FinishedPayload {
    /// Rename into place. Called whether or not the hash matched (mirrors the prior
    /// in-memory decoder's behavior of writing the recovered file before checking/bailing
    /// on a mismatch, so a corrupted result is still left for inspection). On a hard error
    /// before this point (RS failure, I/O error), the temp file is instead cleaned up
    /// automatically by `NamedTempFile`'s `Drop`, leaving no partial output behind.
    pub fn persist(self) -> Result<PathBuf> {
        self.tmp.persist(&self.out_path)?;
        Ok(self.out_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trip() {
        let raw = b"hello framevault payload".to_vec();
        let meta = Meta {
            v: ENCODING_VERSION,
            filename: "x.bin".into(),
            size: raw.len() as u64,
            sha256: sha256_hex(&raw),
        };
        let payload = assemble_payload(&meta, &raw);
        let (pmeta, pdata) = parse_payload(&payload, None).unwrap();
        assert_eq!(pdata, raw);
        assert_eq!(pmeta.filename, "x.bin");
        assert_eq!(pmeta.size, raw.len() as u64);
        assert_eq!(pmeta.sha256, sha256_hex(&raw));
    }

    #[test]
    fn qr_meta_short_and_long_keys() {
        let short = r#"{"v":5,"f":"a.bin","s":10,"h":"abc","e":42,"n":1,"i":16,"m":30}"#;
        let m = parse_qr_meta(short).expect("short keys");
        assert_eq!(m.filename.as_deref(), Some("a.bin"));
        assert_eq!(m.size, Some(10));
        assert_eq!(m.index_bits, Some(16));

        let long = r#"{"version":4,"filename":"b.bin","size":20,"sha256":"def"}"#;
        let m2 = parse_qr_meta(long).expect("long keys");
        assert_eq!(m2.filename.as_deref(), Some("b.bin"));
        assert_eq!(m2.size, Some(20));

        assert!(parse_qr_meta(r#"{"v":4,"f":"x"}"#).is_none()); // missing required fields
        assert!(parse_qr_meta("not json").is_none());
    }

    #[test]
    fn qr_meta_serializes_short_keys() {
        let meta = Meta {
            v: ENCODING_VERSION,
            filename: "a.bin".into(),
            size: 10,
            sha256: "abc".into(),
        };
        let s = String::from_utf8(build_qr_metadata(&meta, 42, 1, 16)).unwrap();
        assert!(s.contains("\"f\":\"a.bin\""));
        assert!(s.contains("\"i\":16"));
        assert!(!s.contains("filename"));
    }
}
