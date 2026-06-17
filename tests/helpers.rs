//! Pure codec-helper tests: frame-index selection and RS round-trips. No media/ffmpeg.

use framevault::constants::*;
use framevault::frame::{frame_layout, select_index_bits};
use framevault::rs::{ecc_encode, ecc_len_for, rs_decode};

// ---------------------------------------------------------------------------
// Frame-index selection (16-bit small / 32-bit large)
// ---------------------------------------------------------------------------

#[test]
fn index_selection_small_files_use_16bit() {
    assert_eq!(select_index_bits(100), 16);
    assert_eq!(select_index_bits(1024 * 1024), 16);
    assert_eq!(select_index_bits(10 * 1024 * 1024), 32);
}

#[test]
fn index_selection_boundary() {
    let (_, data_bits) = frame_layout(DEFAULT_FRAME_INDEX_BITS);
    let bytes_per_frame = data_bits / 8;
    let max_ecc_16 = MAX_FRAME_COUNT_DEFAULT as usize * bytes_per_frame;
    assert_eq!(select_index_bits(max_ecc_16), 16);
    assert_eq!(select_index_bits(max_ecc_16 + 1), 32);
}

#[test]
fn frame_layout_capacity() {
    let (header16, data16) = frame_layout(DEFAULT_FRAME_INDEX_BITS);
    assert_eq!(header16, SYNC_BITS + DEFAULT_FRAME_INDEX_BITS);
    assert_eq!(data16, TOTAL_BLOCKS - header16);
    assert_eq!(data16, 456);
    assert_eq!(data16 % 8, 0);

    let (header32, data32) = frame_layout(EXTENDED_FRAME_INDEX_BITS);
    assert_eq!(header32, SYNC_BITS + EXTENDED_FRAME_INDEX_BITS);
    assert_eq!(data32, TOTAL_BLOCKS - header32);
    assert_eq!(data32, 440);
    assert_eq!(data32 % 8, 0);
}

#[test]
fn max_frame_counts() {
    assert_eq!(MAX_FRAME_COUNT_DEFAULT, 1 << 16);
    assert_eq!(MAX_FRAME_COUNT_EXTENDED, 1u64 << 32);
}

#[test]
fn theoretical_max_ecc() {
    let (_, data16) = frame_layout(DEFAULT_FRAME_INDEX_BITS);
    let max_ecc_16 = MAX_FRAME_COUNT_DEFAULT as usize * (data16 / 8);
    assert_eq!(max_ecc_16, 3_735_552); // 65536 * 57

    let (_, data32) = frame_layout(EXTENDED_FRAME_INDEX_BITS);
    let max_ecc_32 = MAX_FRAME_COUNT_EXTENDED * (data32 as u64 / 8);
    assert_eq!(max_ecc_32, (1u64 << 32) * 55);
    assert!(max_ecc_32 > 200 * 1024 * 1024 * 1024); // > 200 GB
}

#[test]
fn index_selection_at_rs_boundaries() {
    for &size in &[1usize, 100, 1024, 10 * 1024, 100 * 1024] {
        let ecc = ecc_encode(&vec![0u8; size]);
        assert_eq!(select_index_bits(ecc.len()), 16, "expected 16-bit for {size}");
    }
}

// ---------------------------------------------------------------------------
// Reed-Solomon round-trips
// ---------------------------------------------------------------------------

#[test]
fn parallel_rs_round_trip() {
    let payload = b"FrameVault parallel ECC test\n".repeat(200);
    let ecc = ecc_encode(&payload);
    assert_eq!(rs_decode(&ecc, None).unwrap(), payload);
}

#[test]
fn video_only_rs_decode_clean() {
    let payload = b"video-only decode test\n".repeat(100);
    let ecc = ecc_encode(&payload);
    assert_eq!(rs_decode(&ecc, None).unwrap(), payload);
}

fn rs_round_trip(size_bytes: usize) {
    let payload: Vec<u8> = (0..size_bytes).map(|i| (i % 256) as u8).collect();
    let ecc = ecc_encode(&payload);
    assert_eq!(ecc.len(), ecc_len_for(size_bytes), "ecc size {size_bytes}");
    assert_eq!(rs_decode(&ecc, None).unwrap(), payload, "round trip {size_bytes}");
}

#[test]
fn rs_round_trip_sizes() {
    for &n in &[
        1,
        22,
        100,
        1024,
        10 * 1024,
        100 * 1024,
        RS_DATA_BYTES,
        RS_DATA_BYTES - 1,
        RS_DATA_BYTES + 1,
        RS_DATA_BYTES * 7 + 99,
    ] {
        rs_round_trip(n);
    }
}
