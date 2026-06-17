//! Low-level libav round-trip spike: encode real frames to MP4 and decode them back,
//! verifying the video block channel survives losslessly.

use framevault::constants::*;
use framevault::frame::{decode_frame, frame_layout, make_frame_luma, read_blocks};
use framevault::media;

#[test]
fn libav_video_round_trip() {
    let index_bits = DEFAULT_FRAME_INDEX_BITS;
    let (header_bits, dbpf) = frame_layout(index_bits);
    let data_bytes_per_frame = dbpf / 8;
    let n = 8usize;

    // Distinct deterministic packed data bytes per frame.
    let frames_data: Vec<Vec<u8>> = (0..n)
        .map(|f| (0..data_bytes_per_frame).map(|i| ((i + f) * 5 + 1) as u8).collect())
        .collect();

    let tmp = tempfile::tempdir().unwrap();
    let mp4 = tmp.path().join("spike.mp4");

    let frames_for_closure = frames_data.clone();
    media::encode_to_file(&mp4, n, move |idx| {
        Ok(make_frame_luma(idx as u64, &frames_for_closure[idx], index_bits))
    })
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
        assert_eq!(got[f].1, frames_data[f], "frame data bytes {f}");
    }
}
