//! Shared codec constants — the single source of truth for both encode and decode.
//! Mirrors the constants in the original Python `encode.py` / `decode.py`.

// ---- Video ----
pub const FRAME_WIDTH: usize = 1920;
pub const FRAME_HEIGHT: usize = 1080;
pub const FRAME_RATE: usize = 30;
pub const BLOCK_SIZE: usize = 64;
pub const COLS: usize = FRAME_WIDTH / BLOCK_SIZE; // 30
pub const ROWS: usize = FRAME_HEIGHT / BLOCK_SIZE; // 16
pub const TOTAL_BLOCKS: usize = COLS * ROWS; // 480

// ---- Frame layout: [8 sync bits][frame index bits][data bits] ----
pub const SYNC_PATTERN: [u8; 8] = [1, 0, 1, 0, 1, 1, 0, 0];
pub const SYNC_BITS: usize = 8;
pub const DEFAULT_FRAME_INDEX_BITS: usize = 16;
pub const EXTENDED_FRAME_INDEX_BITS: usize = 32;
pub const MAX_FRAME_INDEX: u64 = (1u64 << EXTENDED_FRAME_INDEX_BITS) - 1;
pub const MAX_FRAME_COUNT_DEFAULT: u64 = 1u64 << DEFAULT_FRAME_INDEX_BITS; // 65_536
pub const MAX_FRAME_COUNT_EXTENDED: u64 = MAX_FRAME_INDEX + 1; // 1 << 32

// ---- Reed-Solomon ----
pub const RS_ECC_SYMBOLS: usize = 32; // ECC bytes per 255-byte RS block
pub const RS_DATA_BYTES: usize = 255 - RS_ECC_SYMBOLS; // 223
pub const RS_BLOCK_SIZE: usize = RS_DATA_BYTES + RS_ECC_SYMBOLS; // 255
/// RS data blocks per streaming-encode batch. Each batch reads
/// `RS_DATA_BYTES * RS_STREAM_BATCH_BLOCKS` bytes (~57 KB) and emits ~65 KB of ECC,
/// keeping memory bounded while staying >= `PARALLEL_RS_MIN_BLOCKS` for rayon.
pub const RS_STREAM_BATCH_BLOCKS: usize = 256;

// ---- Parallelism ----
pub const PARALLEL_RS_MIN_BLOCKS: usize = 4;
pub const PARALLEL_CHUNK_FACTOR: usize = 4;
pub const FRAME_BATCH_SIZE: usize = 8;

// ---- Metadata QR ----
pub const METADATA_DURATION_SEC: f64 = 1.0;
/// `max(1, round(FRAME_RATE * METADATA_DURATION_SEC))` = 30. Verified by a unit test.
pub const METADATA_FRAMES: usize = 30;
pub const QR_BORDER_MODULES: usize = 4;
pub const QR_MIN_MODULE_PX: usize = 6;

// ---- Decode sampling ----
/// How many pixels in from each block edge to skip when sampling the clean center.
pub const BLOCK_SAMPLE_MARGIN: usize = 16;
pub const THRESHOLD: f64 = 128.0;

pub const ENCODING_VERSION: u32 = 5;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_constants_match_python() {
        assert_eq!(COLS, 30);
        assert_eq!(ROWS, 16);
        assert_eq!(TOTAL_BLOCKS, 480);
        assert_eq!(RS_DATA_BYTES, 223);
        assert_eq!(RS_BLOCK_SIZE, 255);
        assert_eq!(MAX_FRAME_COUNT_DEFAULT, 1 << 16);
        assert_eq!(MAX_FRAME_COUNT_EXTENDED, 1u64 << 32);
        // max(1, round(30 * 1.0)) == 30
        assert_eq!(
            METADATA_FRAMES,
            ((FRAME_RATE as f64 * METADATA_DURATION_SEC).round() as usize).max(1)
        );
    }
}
