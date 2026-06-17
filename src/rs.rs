//! Reed-Solomon ECC over GF(2^8): 32 ECC symbols per block (RS up to (255, 223)),
//! with erasure support and rayon-parallel block processing.
//!
//! Mirrors the Python `ecc_encode` / `rs_decode`. Wraps the `reed-solomon` crate,
//! which appends parity per block and corrects errors + erasures. Validated for
//! variable-length final blocks and up to 32 erasures per block (see tests).

use rayon::prelude::*;
use reed_solomon::{Decoder, Encoder};

use crate::constants::{PARALLEL_RS_MIN_BLOCKS, RS_BLOCK_SIZE, RS_DATA_BYTES, RS_ECC_SYMBOLS};

/// A Reed-Solomon block could not be recovered (too many errors/erasures, or the
/// block was too short to decode).
#[derive(Debug, Clone)]
pub struct ReedSolomonError(pub String);

impl std::fmt::Display for ReedSolomonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Reed-Solomon decode failed: {}", self.0)
    }
}

impl std::error::Error for ReedSolomonError {}

fn encode_block(block: &[u8]) -> Vec<u8> {
    // Buffer derefs to the full [data | ecc] slice.
    Encoder::new(RS_ECC_SYMBOLS).encode(block)[..].to_vec()
}

/// Reed-Solomon encode `data` into the ECC stream: split into 223-byte data blocks,
/// each followed by 32 parity bytes. The final block may be shorter than 223 bytes
/// (variable-length, like reedsolo).
pub fn ecc_encode(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    let n_blocks = data.len().div_ceil(RS_DATA_BYTES);
    let blocks: Vec<Vec<u8>> = if n_blocks >= PARALLEL_RS_MIN_BLOCKS {
        data.par_chunks(RS_DATA_BYTES).map(encode_block).collect()
    } else {
        data.chunks(RS_DATA_BYTES).map(encode_block).collect()
    };
    blocks.concat()
}

/// Length in bytes of the ECC stream [`ecc_encode`] produces for a `payload_len`-byte
/// payload — a pure function of the length, so the frame layout can be planned before
/// any bytes are read. Each full 223-byte data block becomes 255 bytes; a short final
/// block of `r` bytes becomes `r + 32`.
pub fn ecc_len_for(payload_len: usize) -> usize {
    let full = payload_len / RS_DATA_BYTES;
    let last = payload_len % RS_DATA_BYTES;
    if last == 0 {
        full * RS_BLOCK_SIZE
    } else {
        full * RS_BLOCK_SIZE + last + RS_ECC_SYMBOLS
    }
}

/// Reed-Solomon decode the ECC stream back into the original data. Each 255-byte
/// block is decoded independently; `erasures` are absolute byte positions in the
/// whole stream that are known-missing (mapped to per-block offsets). Returns an
/// error if any block is unrecoverable.
pub fn rs_decode(data: &[u8], erasures: Option<&[usize]>) -> Result<Vec<u8>, ReedSolomonError> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    let decode_block = |(idx, block): (usize, &[u8])| -> Result<Vec<u8>, ReedSolomonError> {
        // A block must carry at least one data byte beyond the 32 ECC bytes;
        // guard against underflow/panic inside the crate on truncated input.
        if block.len() <= RS_ECC_SYMBOLS {
            return Err(ReedSolomonError(format!(
                "block too short to decode ({} bytes <= {} ecc bytes)",
                block.len(),
                RS_ECC_SYMBOLS
            )));
        }
        let block_start = idx * RS_BLOCK_SIZE;
        let end = block_start + block.len();
        let local: Vec<u8> = match erasures {
            Some(positions) => positions
                .iter()
                .filter(|&&p| p >= block_start && p < end)
                .map(|&p| (p - block_start) as u8)
                .collect(),
            None => Vec::new(),
        };
        let erase = if local.is_empty() {
            None
        } else {
            Some(local.as_slice())
        };
        match Decoder::new(RS_ECC_SYMBOLS).correct(block, erase) {
            Ok(buf) => Ok(buf.data().to_vec()),
            Err(e) => Err(ReedSolomonError(format!("{e:?}"))),
        }
    };

    let n_blocks = data.len().div_ceil(RS_BLOCK_SIZE);
    let decoded: Result<Vec<Vec<u8>>, ReedSolomonError> = if n_blocks >= PARALLEL_RS_MIN_BLOCKS {
        data.par_chunks(RS_BLOCK_SIZE)
            .enumerate()
            .map(decode_block)
            .collect()
    } else {
        data.chunks(RS_BLOCK_SIZE)
            .enumerate()
            .map(decode_block)
            .collect()
    };
    Ok(decoded?.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_various_sizes() {
        for &n in &[1usize, 22, 100, 222, 223, 224, 255, 446, 500, 1000, 5000, 100 * 1024] {
            let data: Vec<u8> = (0..n).map(|i| (i % 256) as u8).collect();
            let ecc = ecc_encode(&data);
            assert_eq!(ecc.len(), ecc_len_for(n), "ecc size for {n} bytes");
            let recovered = rs_decode(&ecc, None).expect("decode");
            assert_eq!(recovered, data, "round trip for {n} bytes");
        }
    }

    #[test]
    fn corrects_16_errors_per_block() {
        // RS with 32 ECC symbols corrects up to floor(32/2) = 16 byte errors.
        let data: Vec<u8> = (0..223).map(|i| i as u8).collect();
        let mut ecc = ecc_encode(&data);
        for i in 0..16 {
            ecc[i * 3] ^= 0xFF;
        }
        let recovered = rs_decode(&ecc, None).expect("decode with 16 errors");
        assert_eq!(recovered, data);
    }

    #[test]
    fn corrects_32_erasures_per_block() {
        // With known erasure positions, RS corrects up to 32 (= ecc_len) per block.
        let data: Vec<u8> = (0..223).map(|i| i as u8).collect();
        let mut ecc = ecc_encode(&data);
        let positions: Vec<usize> = (0..32).collect();
        for &p in &positions {
            ecc[p] = 0;
        }
        let recovered = rs_decode(&ecc, Some(&positions)).expect("decode with 32 erasures");
        assert_eq!(recovered, data);
    }

    #[test]
    fn too_many_erasures_fails() {
        let data: Vec<u8> = (0..223).map(|i| i as u8).collect();
        let ecc = ecc_encode(&data);
        let positions: Vec<usize> = (0..33).collect(); // 33 > 32 ecc symbols
        assert!(rs_decode(&ecc, Some(&positions)).is_err());
    }

    #[test]
    fn short_block_errors_not_panics() {
        // A block at/under the ECC length must return an error, never panic.
        assert!(rs_decode(&[0u8; 10], None).is_err());
    }

    #[test]
    fn multi_block_erasures_mapped_per_block() {
        let data: Vec<u8> = (0..600).map(|i| (i % 256) as u8).collect();
        let mut ecc = ecc_encode(&data); // 3 blocks: 255, 255, 186
        let positions = vec![0usize, 5, 17, 255, 260, 280, 510, 515, 540];
        for &p in &positions {
            ecc[p] = 0;
        }
        let recovered = rs_decode(&ecc, Some(&positions)).expect("decode multi-block");
        assert_eq!(recovered, data);
    }
}
