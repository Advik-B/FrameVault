//! Streaming Reed-Solomon ECC: adapts a payload byte stream (`io::Read`) into the ECC
//! byte stream produced by [`ecc_encode`], without materializing either in full.
//!
//! Each refill reads exactly `RS_DATA_BYTES * batch_blocks` payload bytes (a multiple of
//! the 223-byte RS data block) unless the source is exhausted, so only the final batch is
//! ever short. That block-alignment is what guarantees the concatenated streamed output is
//! byte-for-byte identical to `ecc_encode` over the whole payload.

use std::io::{self, Read};

use crate::constants::RS_DATA_BYTES;
use crate::rs::ecc_encode;

/// Wraps a payload reader and yields the ECC stream via [`Read`].
pub struct StreamingEcc<R: Read> {
    inner: R,
    in_buf: Vec<u8>, // reused payload read buffer (one full batch)
    ecc: Vec<u8>,    // pending encoded bytes for the current batch
    pos: usize,      // read cursor into `ecc`
    eof: bool,       // payload source exhausted
}

impl<R: Read> StreamingEcc<R> {
    /// `batch_blocks` (the caller's memory budget in `RS_BLOCK_SIZE` units, see
    /// `budget.rs`; clamped to >= 1) sets how many RS data blocks are read and encoded
    /// per refill.
    pub fn new(inner: R, batch_blocks: usize) -> Self {
        Self {
            inner,
            in_buf: vec![0u8; RS_DATA_BYTES * batch_blocks.max(1)],
            ecc: Vec::new(),
            pos: 0,
            eof: false,
        }
    }

    /// Fill `in_buf` with up to one batch, retrying on `Interrupted` and looping over
    /// short reads so a non-final batch is never truncated. A returned count less than
    /// `in_buf.len()` means the source reached true EOF.
    fn fill_batch(&mut self) -> io::Result<usize> {
        let mut filled = 0;
        while filled < self.in_buf.len() {
            match self.inner.read(&mut self.in_buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }

    /// Read and encode the next batch into `self.ecc`. Returns `false` once there is no
    /// more payload to encode.
    fn refill(&mut self) -> io::Result<bool> {
        if self.eof {
            return Ok(false);
        }
        let n = self.fill_batch()?;
        if n < self.in_buf.len() {
            self.eof = true;
        }
        if n == 0 {
            return Ok(false);
        }
        self.ecc = ecc_encode(&self.in_buf[..n]);
        self.pos = 0;
        Ok(!self.ecc.is_empty())
    }
}

impl<R: Read> Read for StreamingEcc<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.ecc.len() {
            if !self.refill()? {
                return Ok(0);
            }
        }
        let n = (self.ecc.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.ecc[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn stream_all(payload: &[u8], batch_blocks: usize) -> Vec<u8> {
        let mut s = StreamingEcc::new(Cursor::new(payload.to_vec()), batch_blocks);
        let mut out = Vec::new();
        io::copy(&mut s, &mut out).expect("stream ecc");
        out
    }

    #[test]
    fn streaming_matches_one_shot() {
        // Cross a few batch sizes (including the previous hardcoded default of 256) with
        // a range of payload sizes, proving the block-alignment invariant holds regardless
        // of the caller's memory budget.
        for &batch_blocks in &[1usize, 4, 256] {
            let batch = RS_DATA_BYTES * batch_blocks;
            let sizes = [
                0usize,
                1,
                222,
                223,
                224,
                255,
                batch - 1,
                batch,
                batch + 1,
                2 * batch,
                2 * batch + 7,
                3 * batch + RS_DATA_BYTES + 5,
            ];
            for &n in &sizes {
                let payload: Vec<u8> = (0..n).map(|i| (i * 31 + 7) as u8).collect();
                assert_eq!(
                    stream_all(&payload, batch_blocks),
                    ecc_encode(&payload),
                    "ecc mismatch at n={n}, batch_blocks={batch_blocks}"
                );
            }
        }
    }
}
