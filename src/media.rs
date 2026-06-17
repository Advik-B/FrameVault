//! libav (ffmpeg-next) media pipeline — no subprocess.
//!
//! - [`encode_to_file`] muxes generated luma planes (H.264, lossless CRF 0) into a
//!   video-only MP4.
//! - [`decode_video_frames`] streams decoded frames back as tightly-packed rgb24.

use std::path::Path;

use anyhow::{anyhow, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags as ScaleFlags};
use ffmpeg::{codec, encoder, format, frame, media, Dictionary, Packet, Rational};

use crate::constants::*;

const RGB_STRIDE: usize = FRAME_WIDTH * 3;

fn video_enc_tb() -> Rational {
    Rational(1, FRAME_RATE as i32)
}

/// Initialize libav once and quiet its per-frame logging (errors still show).
fn ff_init() -> Result<()> {
    ffmpeg::init()?;
    ffmpeg::log::set_level(ffmpeg::log::Level::Error);
    Ok(())
}

/// Allocate a codec context bound to `codec` so it inherits that codec's own
/// option defaults. A NULL-codec context (e.g. `Context::new`) carries ffmpeg's
/// generic defaults, which makes libx264 reject the settings as "broken".
fn context_with_codec(c: ffmpeg::Codec) -> codec::context::Context {
    unsafe {
        codec::context::Context::wrap(ffmpeg::ffi::avcodec_alloc_context3(c.as_ptr()), None)
    }
}

/// Encode `num_video_frames` luma planes (1920x1080 Y bytes, produced on demand by
/// `make_luma`) into a video-only MP4 at `output`. Frames are fed to the encoder as
/// YUV420P directly with neutral chroma, so there is no RGB->YUV swscale pass.
///
/// `make_luma` is pulled with strictly increasing indices `0..num_video_frames`, so it
/// can drive a streaming generator; returning `Result` lets a mid-stream IO error from
/// that generator propagate out of the muxing loop.
pub fn encode_to_file<F>(output: &Path, num_video_frames: usize, mut make_luma: F) -> Result<()>
where
    F: FnMut(usize) -> Result<Vec<u8>>,
{
    ff_init()?;
    let mut octx = format::output(&output)?;
    let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);

    // ---- video stream + H.264 encoder ----
    let (mut venc, video_idx) = {
        let vcodec = encoder::find(codec::Id::H264)
            .ok_or_else(|| anyhow!("H.264 encoder (libx264) not available"))?;
        let mut ost = octx.add_stream(vcodec)?;
        let idx = ost.index();
        let mut enc = context_with_codec(vcodec).encoder().video()?;
        enc.set_width(FRAME_WIDTH as u32);
        enc.set_height(FRAME_HEIGHT as u32);
        enc.set_format(Pixel::YUV420P);
        enc.set_time_base(video_enc_tb());
        enc.set_frame_rate(Some(Rational(FRAME_RATE as i32, 1)));
        if global_header {
            enc.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let mut opts = Dictionary::new();
        opts.set("crf", "0"); // lossless H.264 for the local source
        opts.set("preset", "ultrafast");
        let enc = enc.open_as_with(vcodec, opts)?;
        ost.set_parameters(&enc);
        (enc, idx)
    };

    octx.write_header()?;
    let video_tb = octx.stream(video_idx).unwrap().time_base();

    for vi in 0..num_video_frames {
        let luma = make_luma(vi)?;
        let mut yuv = frame::Video::new(Pixel::YUV420P, FRAME_WIDTH as u32, FRAME_HEIGHT as u32);
        // Y plane from the generated luma (stride-aware).
        let ys = yuv.stride(0);
        {
            let yd = yuv.data_mut(0);
            for row in 0..FRAME_HEIGHT {
                yd[row * ys..row * ys + FRAME_WIDTH]
                    .copy_from_slice(&luma[row * FRAME_WIDTH..(row + 1) * FRAME_WIDTH]);
            }
        }
        // Neutral chroma (gray) on the half-resolution U/V planes.
        let (cw, ch) = (FRAME_WIDTH / 2, FRAME_HEIGHT / 2);
        for plane in 1..=2 {
            let cs = yuv.stride(plane);
            let cd = yuv.data_mut(plane);
            for row in 0..ch {
                cd[row * cs..row * cs + cw].fill(128);
            }
        }
        yuv.set_pts(Some(vi as i64));
        venc.send_frame(&yuv)?;
        drain_video(&mut venc, &mut octx, video_idx, video_tb)?;
    }

    venc.send_eof()?;
    drain_video(&mut venc, &mut octx, video_idx, video_tb)?;

    octx.write_trailer()?;
    Ok(())
}

fn drain_video(
    enc: &mut encoder::video::Encoder,
    octx: &mut format::context::Output,
    stream_idx: usize,
    stream_tb: Rational,
) -> Result<()> {
    let mut pkt = Packet::empty();
    while enc.receive_packet(&mut pkt).is_ok() {
        pkt.set_stream(stream_idx);
        pkt.rescale_ts(video_enc_tb(), stream_tb);
        pkt.write_interleaved(octx)?;
    }
    Ok(())
}

/// Decode the video stream of `input`, scaling each frame to a tightly-packed
/// 1920x1080 rgb24 buffer and passing it (in order) to `on_frame`.
pub fn decode_video_frames<F>(input: &Path, mut on_frame: F) -> Result<()>
where
    F: FnMut(&[u8]),
{
    ff_init()?;
    // Ignore the container edit list (created for the AAC priming delay) so the
    // demuxer yields every encoded frame, not just the "presented" ones.
    let mut opts = Dictionary::new();
    opts.set("ignore_editlist", "1");
    let mut ictx = format::input_with_dictionary(&input, opts)?;
    let stream = ictx
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| anyhow!("no video stream found"))?;
    let video_idx = stream.index();
    let mut decoder = codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .video()?;
    let mut scaler = Scaler::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        Pixel::RGB24,
        FRAME_WIDTH as u32,
        FRAME_HEIGHT as u32,
        ScaleFlags::BILINEAR,
    )?;
    let mut buf = vec![0u8; FRAME_WIDTH * FRAME_HEIGHT * 3];

    for (s, packet) in ictx.packets() {
        if s.index() == video_idx {
            decoder.send_packet(&packet)?;
            drain_decoded_video(&mut decoder, &mut scaler, &mut buf, &mut on_frame)?;
        }
    }
    decoder.send_eof()?;
    drain_decoded_video(&mut decoder, &mut scaler, &mut buf, &mut on_frame)?;
    Ok(())
}

fn drain_decoded_video<F>(
    decoder: &mut ffmpeg::decoder::Video,
    scaler: &mut Scaler,
    buf: &mut [u8],
    on_frame: &mut F,
) -> Result<()>
where
    F: FnMut(&[u8]),
{
    let mut decoded = frame::Video::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        let mut rgb = frame::Video::empty();
        scaler.run(&decoded, &mut rgb)?;
        let stride = rgb.stride(0);
        let data = rgb.data(0);
        for y in 0..FRAME_HEIGHT {
            buf[y * RGB_STRIDE..(y + 1) * RGB_STRIDE]
                .copy_from_slice(&data[y * stride..y * stride + RGB_STRIDE]);
        }
        on_frame(buf);
    }
    Ok(())
}

