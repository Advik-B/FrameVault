//! Port of the Python `test_codec_helpers.py`: pure codec-helper tests (audio
//! position layout, frame-index selection, RS round-trips, audio merge/repair).
//! No media/ffmpeg involved.

use framevault::audio::{compute_audio_positions, merge_audio_bytes, select_audio_bytes};
use framevault::constants::*;
use framevault::frame::{frame_layout, frames_needed, select_index_bits};
use framevault::rs::{ecc_encode, rs_decode};

const DIST: Option<&str> = Some(AUDIO_LAYOUT_DISTRIBUTED);
const PREFIX: Option<&str> = Some(AUDIO_LAYOUT_PREFIX);

/// Mirror of the Python `_expected_ecc_size`.
fn expected_ecc_size(n: usize) -> usize {
    let full = n / RS_DATA_BYTES;
    let last = n % RS_DATA_BYTES;
    if last == 0 {
        full * RS_BLOCK_SIZE
    } else {
        full * RS_BLOCK_SIZE + last + RS_ECC_SYMBOLS
    }
}

// ---------------------------------------------------------------------------
// Audio layout
// ---------------------------------------------------------------------------

#[test]
fn distributed_positions_cover_full_range() {
    let pos = compute_audio_positions(1000, 25, DIST);
    assert_eq!(pos.len(), 25);
    assert_eq!(pos[0], 0);
    assert_eq!(*pos.last().unwrap(), 999);
    assert!(pos.windows(2).all(|w| w[1] > w[0]));
}

#[test]
fn positions_cover_all_bytes_when_full_audio() {
    let pos = compute_audio_positions(128, 128, DIST);
    assert_eq!(pos, (0..128).collect::<Vec<_>>());
}

#[test]
fn positions_single_byte_midpoint() {
    assert_eq!(compute_audio_positions(101, 1, DIST), vec![101 / 2]);
    assert_eq!(compute_audio_positions(100, 1, DIST), vec![100 / 2]);
}

#[test]
fn merge_audio_bytes_uses_planned_positions() {
    let ecc: Vec<u8> = (0..200u8).collect();
    let planned = 10;
    let audio = select_audio_bytes(&ecc, planned, DIST);
    let blank = vec![0u8; ecc.len()];
    let (merged, positions) = merge_audio_bytes(&blank, &audio, planned, DIST);
    for (i, &p) in positions.iter().enumerate() {
        assert_eq!(merged[p], audio[i]);
    }
    assert_eq!(positions.len(), planned);
}

#[test]
fn parallel_rs_round_trip() {
    let payload = b"FrameVault parallel ECC test\n".repeat(200);
    let ecc = ecc_encode(&payload);
    assert_eq!(rs_decode(&ecc, None).unwrap(), payload);
}

#[test]
fn distributed_audio_merge_repairs_corrupted_bytes() {
    let payload = b"distributed audio repair\n".repeat(150);
    let ecc = ecc_encode(&payload);
    let planned = 24;
    let audio = select_audio_bytes(&ecc, planned, DIST);
    let positions = compute_audio_positions(ecc.len(), planned, DIST);

    let mut corrupted = ecc.clone();
    for &p in &positions {
        corrupted[p] ^= 0x5A;
    }
    let (repaired, recovered) = merge_audio_bytes(&corrupted, &audio, planned, DIST);
    assert_eq!(recovered, positions);
    assert_eq!(rs_decode(&repaired, None).unwrap(), payload);
}

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

// ---------------------------------------------------------------------------
// Audio positions span the entire ECC stream
// ---------------------------------------------------------------------------

fn assert_positions(ecc_len: usize, audio_bytes: usize) {
    let pos = compute_audio_positions(ecc_len, audio_bytes, DIST);
    let n = audio_bytes.min(ecc_len);
    assert_eq!(pos.len(), n, "count for ecc_len={ecc_len}, audio_bytes={audio_bytes}");
    if n >= 2 {
        assert_eq!(pos[0], 0, "first anchor ecc_len={ecc_len} audio={audio_bytes}");
        assert_eq!(*pos.last().unwrap(), ecc_len - 1, "last anchor ecc_len={ecc_len}");
        assert!(
            pos.windows(2).all(|w| w[1] > w[0]),
            "monotone ecc_len={ecc_len} audio={audio_bytes}"
        );
        let mut sorted = pos.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), n, "unique ecc_len={ecc_len} audio={audio_bytes}");
    }
}

#[test]
fn audio_positions_full_range_cases() {
    assert_positions(10, 5);
    assert_positions(100, 2);
    assert_positions(50, 50);
    assert_positions(1143, 25);
    assert_positions(50_000, 1250);
    assert_positions(3_735_552, 54_637); // full-size 16-bit
}

#[test]
fn audio_exceeds_ecc_clamps() {
    assert_eq!(compute_audio_positions(30, 100, DIST).len(), 30);
}

#[test]
fn single_byte_audio_anchors_midpoint() {
    assert_eq!(compute_audio_positions(101, 1, DIST)[0], 50);
    assert_eq!(compute_audio_positions(100, 1, DIST)[0], 50);
}

#[test]
fn prefix_layout_covers_prefix() {
    assert_eq!(compute_audio_positions(1000, 25, PREFIX), (0..25).collect::<Vec<_>>());
}

#[test]
fn zero_inputs_return_empty() {
    assert!(compute_audio_positions(1000, 0, DIST).is_empty());
    assert!(compute_audio_positions(0, 25, DIST).is_empty());
}

#[test]
fn audio_positions_random_inputs() {
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..60 {
        let ecc_len = 2 + (next() % 100_000) as usize;
        let audio_bytes = 1 + (next() % ecc_len as u64) as usize;
        assert_positions(ecc_len, audio_bytes);
    }
}

// ---------------------------------------------------------------------------
// Audio independence — video-only RS works without audio
// ---------------------------------------------------------------------------

#[test]
fn video_only_rs_decode_clean() {
    let payload = b"video-only decode test\n".repeat(100);
    let ecc = ecc_encode(&payload);
    assert_eq!(rs_decode(&ecc, None).unwrap(), payload);
}

#[test]
fn empty_audio_leaves_ecc_unchanged() {
    let ecc: Vec<u8> = (0..200u8).collect();
    let (merged, positions) = merge_audio_bytes(&ecc, b"", 10, DIST);
    assert_eq!(merged, ecc);
    assert!(positions.is_empty());

    let (merged0, positions0) = merge_audio_bytes(&ecc, b"", 0, None);
    assert_eq!(merged0, ecc);
    assert!(positions0.is_empty());
}

#[test]
fn audio_merge_on_clean_ecc_still_decodes() {
    let payload = b"clean ecc with audio overlay\n".repeat(80);
    let ecc = ecc_encode(&payload);
    let audio_count = 50;
    let audio = select_audio_bytes(&ecc, audio_count, DIST);
    let (merged, _) = merge_audio_bytes(&ecc, &audio, audio_count, DIST);
    assert_eq!(rs_decode(&merged, None).unwrap(), payload);
}

#[test]
fn audio_recovery_repairs_targeted_corruption() {
    let payload = b"targeted corruption repair\n".repeat(120);
    let ecc = ecc_encode(&payload);
    let audio_count = 30;
    let audio = select_audio_bytes(&ecc, audio_count, DIST);
    let positions = compute_audio_positions(ecc.len(), audio_count, DIST);

    let mut corrupted = ecc.clone();
    for &p in &positions {
        corrupted[p] ^= 0xFF;
    }
    let (repaired, _) = merge_audio_bytes(&corrupted, &audio, audio_count, DIST);
    assert_eq!(rs_decode(&repaired, None).unwrap(), payload);
}

#[test]
fn merge_with_partial_audio_coverage() {
    let ecc_len = 500;
    let ecc: Vec<u8> = (0..ecc_len).map(|i| (i % 256) as u8).collect();
    let planned_audio = 20;
    let actual_audio = 5;
    let audio = select_audio_bytes(&ecc, planned_audio, DIST)[..actual_audio].to_vec();
    let (merged, recovered) = merge_audio_bytes(&ecc, &audio, planned_audio, DIST);
    assert_eq!(recovered.len(), actual_audio);
    for (i, &p) in recovered.iter().enumerate() {
        assert_eq!(merged[p], audio[i]);
    }
}

// ---------------------------------------------------------------------------
// RS round-trip at various input sizes
// ---------------------------------------------------------------------------

fn rs_round_trip(size_bytes: usize) {
    let payload: Vec<u8> = (0..size_bytes).map(|i| (i % 256) as u8).collect();
    let ecc = ecc_encode(&payload);
    assert_eq!(ecc.len(), expected_ecc_size(size_bytes), "ecc size {size_bytes}");
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

#[test]
fn audio_byte_count_scales_with_ecc_size() {
    let (_, data_bits) = frame_layout(DEFAULT_FRAME_INDEX_BITS);
    for &raw_size in &[1usize, 50, 500, 5000] {
        let ecc = ecc_encode(&vec![0u8; raw_size]);
        let frames = frames_needed(ecc.len() * 8, data_bits);
        let duration_sec = (frames + METADATA_FRAMES) as f64 / FRAME_RATE as f64;
        let audio_count = ecc.len().min((duration_sec * BYTES_PER_SEC_AUDIO as f64) as usize);
        assert!(audio_count <= ecc.len(), "audio exceeds ecc for {raw_size}");
    }
}

#[test]
fn index_selection_at_rs_boundaries() {
    for &size in &[1usize, 100, 1024, 10 * 1024, 100 * 1024] {
        let ecc = ecc_encode(&vec![0u8; size]);
        assert_eq!(select_index_bits(ecc.len()), 16, "expected 16-bit for {size}");
    }
}
