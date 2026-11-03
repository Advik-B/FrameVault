//! Port of the Python `test_roundtrip.py`: real encode -> MP4 -> decode -> verify
//! round-trips through the libav pipeline.

use framevault::decode::{decode, DecodeReport, Strategy};
use framevault::encode::{encode, EncodeReport};
use framevault::media::{encoder_available, Preset, VideoCodec};

/// Deterministic pseudo-random bytes (xorshift64).
fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s & 0xff) as u8
        })
        .collect()
}

fn round_trip_with(
    data: &[u8],
    filename: &str,
    memory: Option<&str>,
    codec: VideoCodec,
    preset: Preset,
) -> (Vec<u8>, EncodeReport, DecodeReport) {
    let tmp = tempfile::tempdir().unwrap();
    let infile = tmp.path().join(filename);
    let video = tmp.path().join("out.mp4");
    let recdir = tmp.path().join("rec");
    std::fs::create_dir_all(&recdir).unwrap();
    std::fs::write(&infile, data).unwrap();

    let enc = encode(&infile, &video, memory, codec, preset).expect("encode");
    assert!(video.exists(), "encoder produced no MP4");
    let dec = decode(&video, &recdir, memory).expect("decode");
    let recovered = std::fs::read(recdir.join(filename)).expect("recovered file missing");
    (recovered, enc, dec)
}

fn round_trip(data: &[u8], filename: &str) -> (Vec<u8>, EncodeReport, DecodeReport) {
    round_trip_with(data, filename, None, VideoCodec::H264, Preset::default())
}

fn assert_round_trip(data: &[u8], filename: &str) {
    let (recovered, enc, dec) = round_trip(data, filename);
    assert_eq!(recovered, data, "byte-level mismatch for {filename}");
    assert!(dec.sha_ok, "SHA mismatch for {filename}");
    assert_eq!(enc.index_bits, 16, "expected 16-bit index for {filename}");
    assert_eq!(
        dec.strategy,
        Strategy::VideoOnly,
        "clean local video should decode video-only for {filename}"
    );
}

// ---- size sweep (16-bit index selection) ----

#[test]
fn size_1_byte() {
    assert_round_trip(&[0xAB], "one.bin");
}

#[test]
fn size_22_bytes() {
    assert_round_trip(&(0u8..22).collect::<Vec<_>>(), "threshold.bin");
}

#[test]
fn size_100_bytes() {
    assert_round_trip(&pseudo_random(100, 1), "r100.bin");
}

#[test]
fn size_1kb() {
    assert_round_trip(&pseudo_random(1024, 2), "r1k.bin");
}

#[test]
fn size_10kb() {
    assert_round_trip(&pseudo_random(10 * 1024, 3), "r10k.bin");
}

// Heavy; run with `cargo test --release -- --ignored`.
#[test]
#[ignore]
fn size_100kb() {
    assert_round_trip(&pseudo_random(100 * 1024, 4), "r100k.bin");
}

// ---- data variety ----

#[test]
fn variety_all_zeros() {
    assert_round_trip(&vec![0u8; 2048], "zeros.bin");
}

#[test]
fn variety_all_0xff() {
    assert_round_trip(&vec![0xFFu8; 2048], "ones.bin");
}

#[test]
fn variety_text() {
    let data = "Hello FrameVault!\nThis is a test.\n".repeat(60).into_bytes();
    assert_round_trip(&data, "readme.txt");
}

#[test]
fn variety_repeating_256_cycle() {
    let cycle: Vec<u8> = (0..=255u8).collect();
    let data: Vec<u8> = cycle.repeat(12);
    assert_round_trip(&data, "pattern.bin");
}

#[test]
fn variety_mixed_binary() {
    let mut data = vec![0u8; 1024];
    data.extend(vec![0xFFu8; 512]);
    data.extend(pseudo_random(512, 7));
    assert_round_trip(&data, "mixed.bin");
}

// ---- memory budget ----

// A deliberately tiny explicit `--memory` forces many RS batch/window rotations on both
// the encode and decode sides through the real MP4 pipeline, proving a small budget
// doesn't break correctness.
#[test]
fn tiny_memory_budget_round_trip() {
    let data = pseudo_random(20 * 1024, 9);
    let (recovered, enc, dec) =
        round_trip_with(&data, "tiny_budget.bin", Some("1K"), VideoCodec::H264, Preset::Fast);
    assert_eq!(recovered, data, "byte-level mismatch under a tiny memory budget");
    assert!(dec.sha_ok, "SHA mismatch under a tiny memory budget");
    assert_eq!(enc.index_bits, 16);
}

// ---- codecs ----

// H.265 round trip through libx265's lossless mode. Skipped (with a note) when the linked
// FFmpeg lacks libx265 — e.g. a Windows vcpkg build from before `x265` was added to
// scripts/setup-windows.ps1 (rerun it to rebuild).
#[test]
fn h265_round_trip() {
    if !encoder_available(VideoCodec::H265) {
        eprintln!("skipping h265_round_trip: libx265 not available in this FFmpeg build");
        return;
    }
    let data = pseudo_random(4 * 1024, 11);
    for preset in [Preset::Ultrafast, Preset::Fast, Preset::Medium] {
        let (recovered, _enc, dec) =
            round_trip_with(&data, "hevc.bin", None, VideoCodec::H265, preset);
        assert_eq!(recovered, data, "byte-level mismatch through H.265 at {preset}");
        assert!(dec.sha_ok, "SHA mismatch through H.265 at {preset}");
    }
}

// Every preset stays lossless: slower ones only shrink the file. The B-frames that
// `fast` and slower presets enable reorder frames in the stream, which the decoder must
// handle; `ultrafast` has none.
#[test]
fn h264_presets_round_trip() {
    let data = pseudo_random(3 * 1024, 13);
    let mut sizes = Vec::new();
    for preset in [Preset::Ultrafast, Preset::Fast, Preset::Veryslow] {
        let tmp = tempfile::tempdir().unwrap();
        let infile = tmp.path().join("p.bin");
        let video = tmp.path().join("p.mp4");
        std::fs::write(&infile, &data).unwrap();
        encode(&infile, &video, None, VideoCodec::H264, preset).expect("encode");
        sizes.push(std::fs::metadata(&video).unwrap().len());
        let recdir = tmp.path().join("rec");
        std::fs::create_dir_all(&recdir).unwrap();
        let dec = decode(&video, &recdir, None).expect("decode");
        assert!(dec.sha_ok, "SHA mismatch at preset {preset}");
        assert_eq!(std::fs::read(&dec.output_path).unwrap(), data, "mismatch at {preset}");
    }
    assert!(sizes[1] < sizes[0], "fast should beat ultrafast on size: {sizes:?}");
}
