//! Port of the Python `test_roundtrip.py`: real encode -> MP4 -> decode -> verify
//! round-trips through the libav pipeline, plus audio-independence checks.

use framevault::decode::{decode, DecodeReport, Strategy};
use framevault::encode::{encode, EncodeReport};
use framevault::media;

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

fn round_trip(data: &[u8], filename: &str) -> (Vec<u8>, EncodeReport, DecodeReport) {
    let tmp = tempfile::tempdir().unwrap();
    let infile = tmp.path().join(filename);
    let video = tmp.path().join("out.mp4");
    let recdir = tmp.path().join("rec");
    std::fs::create_dir_all(&recdir).unwrap();
    std::fs::write(&infile, data).unwrap();

    let enc = encode(&infile, &video).expect("encode");
    assert!(video.exists(), "encoder produced no MP4");
    let dec = decode(&video, &recdir).expect("decode");
    let recovered = std::fs::read(recdir.join(filename)).expect("recovered file missing");
    (recovered, enc, dec)
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

// ---- audio coverage reported by the encoder ----

#[test]
fn encode_reports_distributed_audio_coverage() {
    let (_, enc, _) = round_trip(&pseudo_random(2048, 20), "cov.bin");
    assert_eq!(enc.audio_layout, "distributed");
    assert!(enc.audio_byte_count > 0);
    assert!(enc.audio_byte_count <= enc.ecc_len);
}

// ---- audio independence: video-only decode works without any audio ----

#[test]
fn decodes_with_no_audio_track() {
    let data = pseudo_random(2048, 30);
    let tmp = tempfile::tempdir().unwrap();
    let infile = tmp.path().join("na.bin");
    let video = tmp.path().join("orig.mp4");
    let stripped = tmp.path().join("noaudio.mp4");
    let recdir = tmp.path().join("rec");
    std::fs::create_dir_all(&recdir).unwrap();
    std::fs::write(&infile, &data).unwrap();

    encode(&infile, &video).expect("encode");
    media::remux_drop_audio(&video, &stripped).expect("strip audio");
    let dec = decode(&stripped, &recdir).expect("decode no-audio");

    assert_eq!(dec.strategy, Strategy::VideoOnly);
    assert!(dec.sha_ok);
    assert_eq!(std::fs::read(recdir.join("na.bin")).unwrap(), data);
}
