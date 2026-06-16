//! Payload framing and QR metadata. Mirrors the payload/QR helpers in
//! encode.py / decode.py.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::constants::{AUDIO_LAYOUT, ENCODING_VERSION, METADATA_FRAMES};

/// Self-describing payload metadata header (stored before the raw file bytes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub v: u32,
    pub filename: String,
    pub size: u64,
    pub sha256: String,
    pub audio_layout: String,
}

/// Lowercase hex SHA-256 digest.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(data) {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Build the pre-ECC payload for a file: `[4-byte BE meta length][meta JSON][raw bytes]`.
pub fn build_payload(path: &Path) -> std::io::Result<(Vec<u8>, Meta)> {
    let raw = std::fs::read(path)?;
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file.bin")
        .to_string();
    let meta = Meta {
        v: ENCODING_VERSION,
        filename,
        size: raw.len() as u64,
        sha256: sha256_hex(&raw),
        audio_layout: AUDIO_LAYOUT.to_string(),
    };
    Ok((assemble_payload(&meta, &raw), meta))
}

/// Assemble a payload from already-loaded metadata and raw bytes.
pub fn assemble_payload(meta: &Meta, raw: &[u8]) -> Vec<u8> {
    let meta_bytes = serde_json::to_vec(meta).expect("serialize meta");
    let mut payload = Vec::with_capacity(4 + meta_bytes.len() + raw.len());
    payload.extend_from_slice(&(meta_bytes.len() as u32).to_be_bytes());
    payload.extend_from_slice(&meta_bytes);
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
    #[serde(rename = "a", alias = "audio_bytes", skip_serializing_if = "Option::is_none")]
    pub audio_bytes: Option<u64>,
    #[serde(rename = "p", alias = "audio_layout", skip_serializing_if = "Option::is_none")]
    pub audio_layout: Option<String>,
}

/// Build the compact QR metadata JSON bytes for the decoder.
pub fn build_qr_metadata(
    meta: &Meta,
    ecc_len: usize,
    num_frames: u64,
    index_bits: usize,
    audio_bytes: usize,
    audio_layout: &str,
) -> Vec<u8> {
    let qr = QrMeta {
        version: Some(ENCODING_VERSION),
        filename: Some(meta.filename.clone()),
        size: Some(meta.size),
        sha256: Some(meta.sha256.clone()),
        ecc_bytes: Some(ecc_len as u64),
        frames: Some(num_frames),
        index_bits: Some(index_bits as u32),
        metadata_frames: Some(METADATA_FRAMES as u64),
        audio_bytes: Some(audio_bytes as u64),
        audio_layout: Some(audio_layout.to_string()),
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
            audio_layout: AUDIO_LAYOUT.into(),
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
        let short = r#"{"v":4,"f":"a.bin","s":10,"h":"abc","e":42,"n":1,"i":16,"m":30,"a":25,"p":"distributed"}"#;
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
            v: 4,
            filename: "a.bin".into(),
            size: 10,
            sha256: "abc".into(),
            audio_layout: "distributed".into(),
        };
        let s = String::from_utf8(build_qr_metadata(&meta, 42, 1, 16, 25, "distributed")).unwrap();
        assert!(s.contains("\"f\":\"a.bin\""));
        assert!(s.contains("\"i\":16"));
        assert!(!s.contains("filename"));
    }
}
