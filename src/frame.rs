//! Video channel: frame layout, big-endian frame indexing, bit<->pixel rendering,
//! and block-center sampling. Mirrors the frame functions in encode.py / decode.py.

use crate::constants::*;

/// Returns `(header_bits, data_bits_per_frame)` for a given frame-index width.
/// The data region must byte-align (true for both the 16- and 32-bit widths).
pub fn frame_layout(index_bits: usize) -> (usize, usize) {
    let header_bits = SYNC_BITS + index_bits;
    let data_bits_per_frame = TOTAL_BLOCKS - header_bits;
    assert!(
        data_bits_per_frame > 0 && data_bits_per_frame.is_multiple_of(8),
        "frame index bits {index_bits} do not byte-align the data region"
    );
    (header_bits, data_bits_per_frame)
}

/// Number of frames needed to carry `total_bits` at `data_bits_per_frame` each.
pub fn frames_needed(total_bits: usize, data_bits_per_frame: usize) -> usize {
    total_bits.div_ceil(data_bits_per_frame)
}

/// Choose the frame-index width for an ECC stream of `ecc_len` bytes: 16-bit
/// while the data fits within 65,536 frames, otherwise 32-bit. Mirrors the
/// encoder's selection in encode.py.
pub fn select_index_bits(ecc_len: usize) -> usize {
    let total_bits = ecc_len * 8;
    let (_, data_bits_default) = frame_layout(DEFAULT_FRAME_INDEX_BITS);
    let frames_16 = frames_needed(total_bits, data_bits_default) as u64;
    if frames_16 <= MAX_FRAME_COUNT_DEFAULT {
        DEFAULT_FRAME_INDEX_BITS
    } else {
        EXTENDED_FRAME_INDEX_BITS
    }
}

/// Big-endian (MSB first) bit expansion of a frame index.
pub fn index_to_bits(idx: u64, index_bits: usize) -> Vec<u8> {
    (0..index_bits)
        .map(|i| ((idx >> (index_bits - 1 - i)) & 1) as u8)
        .collect()
}

/// Inverse of [`index_to_bits`] (big-endian).
pub fn bits_to_index(bits: &[u8]) -> u64 {
    bits.iter().fold(0u64, |acc, &b| (acc << 1) | (b as u64 & 1))
}

/// Expand packed `src` bytes (MSB first) into one-bit-per-element, filling `dst`.
/// `dst.len()` must equal `src.len() * 8`.
fn unpack_bits_into(dst: &mut [u8], src: &[u8]) {
    debug_assert_eq!(dst.len(), src.len() * 8);
    for (j, &byte) in src.iter().enumerate() {
        for k in 0..8 {
            dst[j * 8 + k] = (byte >> (7 - k)) & 1;
        }
    }
}

/// Render a data frame as a 1920x1080 rgb24 byte buffer: an 8-bit sync pattern,
/// the frame index, and `data_bytes` (packed, MSB first) laid out over a 30x16 grid
/// of 64x64 black/white blocks. The bottom 56 px (1080 - 16*64) stay black.
pub fn make_frame(frame_idx: u64, data_bytes: &[u8], index_bits: usize) -> Vec<u8> {
    let header_bits = SYNC_BITS + index_bits;
    debug_assert_eq!(data_bytes.len() * 8, TOTAL_BLOCKS - header_bits);

    let mut block_bits = [0u8; TOTAL_BLOCKS];
    block_bits[..SYNC_BITS].copy_from_slice(&SYNC_PATTERN);
    block_bits[SYNC_BITS..header_bits].copy_from_slice(&index_to_bits(frame_idx, index_bits));
    unpack_bits_into(&mut block_bits[header_bits..], data_bytes);

    let mut frame = vec![0u8; FRAME_WIDTH * FRAME_HEIGHT * 3];
    for r in 0..ROWS {
        for c in 0..COLS {
            if block_bits[r * COLS + c] == 0 {
                continue; // black block: buffer is already zeroed
            }
            for by in 0..BLOCK_SIZE {
                let y = r * BLOCK_SIZE + by;
                let off = (y * FRAME_WIDTH + c * BLOCK_SIZE) * 3;
                frame[off..off + BLOCK_SIZE * 3].fill(255);
            }
        }
    }
    frame
}

/// Luma (Y-plane) values for black/white blocks under limited-range BT.601 —
/// what RGB 0 / 255 map to through swscale. Used to feed the encoder YUV420P
/// directly (no RGB buffer, no RGB->YUV scale pass).
pub const BLACK_Y: u8 = 16;
pub const WHITE_Y: u8 = 235;

/// Render a data frame's 1920x1080 luma plane directly (single channel) from
/// `data_bytes` (packed, MSB first). Bottom padding rows stay black. Equivalent to
/// [`make_frame`]'s R channel but ~3x less data and no swscale needed on encode.
pub fn make_frame_luma(frame_idx: u64, data_bytes: &[u8], index_bits: usize) -> Vec<u8> {
    let header_bits = SYNC_BITS + index_bits;
    debug_assert_eq!(data_bytes.len() * 8, TOTAL_BLOCKS - header_bits);

    let mut block_bits = [0u8; TOTAL_BLOCKS];
    block_bits[..SYNC_BITS].copy_from_slice(&SYNC_PATTERN);
    block_bits[SYNC_BITS..header_bits].copy_from_slice(&index_to_bits(frame_idx, index_bits));
    unpack_bits_into(&mut block_bits[header_bits..], data_bytes);

    let mut luma = vec![BLACK_Y; FRAME_WIDTH * FRAME_HEIGHT];
    for r in 0..ROWS {
        for c in 0..COLS {
            if block_bits[r * COLS + c] == 0 {
                continue;
            }
            for by in 0..BLOCK_SIZE {
                let off = (r * BLOCK_SIZE + by) * FRAME_WIDTH + c * BLOCK_SIZE;
                luma[off..off + BLOCK_SIZE].fill(WHITE_Y);
            }
        }
    }
    luma
}

/// Sample the clean center (`BLOCK_SAMPLE_MARGIN` inset) of each 64x64 block's
/// R channel and threshold to a flat 480-element bit array.
pub fn read_blocks(frame: &[u8]) -> [u8; TOTAL_BLOCKS] {
    let m = BLOCK_SAMPLE_MARGIN;
    let inner = BLOCK_SIZE - 2 * m; // 32
    let area = (inner * inner) as f64;
    let mut bits = [0u8; TOTAL_BLOCKS];
    for r in 0..ROWS {
        for c in 0..COLS {
            let mut sum = 0u64;
            for yy in 0..inner {
                let y = r * BLOCK_SIZE + m + yy;
                let mut off = (y * FRAME_WIDTH + c * BLOCK_SIZE + m) * 3;
                for _ in 0..inner {
                    sum += frame[off] as u64; // R channel only
                    off += 3;
                }
            }
            bits[r * COLS + c] = if (sum as f64 / area) >= THRESHOLD { 1 } else { 0 };
        }
    }
    bits
}

/// Validate the sync pattern and split a 480-bit frame into `(index, data_bytes)`,
/// with the byte-aligned data region packed MSB first. Returns `None` if the leading
/// sync pattern does not match.
pub fn decode_frame(bits: &[u8], _index_bits: usize, header_bits: usize) -> Option<(u64, Vec<u8>)> {
    if bits[..SYNC_BITS] != SYNC_PATTERN[..] {
        return None;
    }
    let frame_idx = bits_to_index(&bits[SYNC_BITS..header_bits]);
    Some((frame_idx, pack_bits(&bits[header_bits..])))
}

/// Pack a bit array (MSB first) into bytes, dropping any trailing partial byte.
fn pack_bits(bits: &[u8]) -> Vec<u8> {
    let n_bytes = bits.len() / 8;
    (0..n_bytes)
        .map(|b| (0..8).fold(0u8, |acc, k| (acc << 1) | (bits[b * 8 + k] & 1)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_values() {
        assert_eq!(frame_layout(DEFAULT_FRAME_INDEX_BITS), (24, 456));
        assert_eq!(frame_layout(EXTENDED_FRAME_INDEX_BITS), (40, 440));
    }

    #[test]
    fn index_bits_round_trip() {
        for &bits in &[16usize, 32] {
            for &idx in &[0u64, 1, 42, 65535, (1u64 << bits) - 1] {
                let v = index_to_bits(idx, bits);
                assert_eq!(v.len(), bits);
                assert_eq!(bits_to_index(&v), idx);
            }
        }
    }

    #[test]
    fn frame_render_read_round_trip() {
        for &index_bits in &[16usize, 32] {
            let (header_bits, data_bits_per_frame) = frame_layout(index_bits);
            let data: Vec<u8> = (0..data_bits_per_frame / 8).map(|i| (i * 7 + 3) as u8).collect();
            for &idx in &[0u64, 1, 1234, 65535] {
                let frame = make_frame(idx, &data, index_bits);
                assert_eq!(frame.len(), FRAME_WIDTH * FRAME_HEIGHT * 3);
                let bits = read_blocks(&frame);
                let (didx, ddata) = decode_frame(&bits, index_bits, header_bits).expect("sync");
                assert_eq!(didx, idx);
                assert_eq!(ddata, data);
            }
        }
    }

    #[test]
    fn corrupted_sync_rejected() {
        let (header_bits, dbpf) = frame_layout(16);
        let mut bits = vec![0u8; TOTAL_BLOCKS];
        bits[..SYNC_BITS].copy_from_slice(&SYNC_PATTERN);
        let _ = dbpf;
        bits[0] ^= 1; // break sync
        assert!(decode_frame(&bits, 16, header_bits).is_none());
    }
}
