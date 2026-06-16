//! Low-level libav round-trip spike: encode real frames + a tone to MP4 and
//! decode them back, verifying the video block channel survives losslessly.

use framevault::audio::{decode_fsk, make_audio_pcm};
use framevault::constants::*;
use framevault::frame::{decode_frame, frame_layout, make_frame_luma, read_blocks};
use framevault::media;

#[test]
fn libav_video_audio_round_trip() {
    let index_bits = DEFAULT_FRAME_INDEX_BITS;
    let (header_bits, dbpf) = frame_layout(index_bits);
    let n = 8usize;

    // Distinct deterministic data bits per frame.
    let frames_data: Vec<Vec<u8>> = (0..n)
        .map(|f| (0..dbpf).map(|i| (((i + f) * 5 + 1) % 2) as u8).collect())
        .collect();

    // Audio: carry some bytes; give it enough duration to emit all symbols.
    let audio_data: Vec<u8> = (0..30u8).map(|i| i.wrapping_mul(7).wrapping_add(1)).collect();
    let duration = (audio_data.len() * 4 * SAMPLES_PER_SYMBOL) as f64 / SAMPLE_RATE as f64 + 0.2;
    let pcm = make_audio_pcm(&audio_data, duration);

    let tmp = tempfile::tempdir().unwrap();
    let mp4 = tmp.path().join("spike.mp4");

    let frames_for_closure = frames_data.clone();
    media::encode_to_file(
        &mp4,
        n,
        move |idx| make_frame_luma(idx as u64, &frames_for_closure[idx], index_bits),
        &pcm,
    )
    .expect("encode");

    assert!(mp4.exists() && std::fs::metadata(&mp4).unwrap().len() > 0);

    // Decode video frames back and verify each survives losslessly.
    let mut got: Vec<(u64, Vec<u8>)> = Vec::new();
    media::decode_video_frames(&mp4, |frame| {
        let bits = read_blocks(frame);
        if let Some(decoded) = decode_frame(&bits, index_bits, header_bits) {
            got.push(decoded);
        }
    })
    .expect("decode video");

    assert_eq!(got.len(), n, "decoded frame count");
    for f in 0..n {
        assert_eq!(got[f].0, f as u64, "frame index {f}");
        assert_eq!(got[f].1, frames_data[f], "frame data bits {f}");
    }

    // Audio is the fuzzy fallback channel: just confirm it decodes to samples
    // and the FSK demodulator runs without panicking.
    let samples = media::decode_audio_samples(&mp4).expect("decode audio");
    assert!(!samples.is_empty(), "audio samples present");
    let _ = decode_fsk(&samples);
}
