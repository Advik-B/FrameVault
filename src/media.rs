//! libav (ffmpeg-next) media pipeline — no subprocess.
//!
//! - [`encode_to_file`] muxes generated rgb24 frames (H.264, lossless CRF 0) and
//!   mono PCM (AAC 320k) into an MP4, interleaving packets in time order.
//! - [`decode_video_frames`] streams decoded frames back as tightly-packed rgb24.
//! - [`decode_audio_samples`] returns the mono 44.1 kHz track as normalized f32.
//! - [`remux_drop_audio`] copies just the video stream (used by tests).

use std::path::Path;

use anyhow::{anyhow, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags as ScaleFlags};
use ffmpeg::{channel_layout::ChannelLayout, codec, encoder, format, frame, media, Dictionary, Packet, Rational};

use crate::constants::*;

const RGB_STRIDE: usize = FRAME_WIDTH * 3;

fn video_enc_tb() -> Rational {
    Rational(1, FRAME_RATE as i32)
}

fn audio_enc_tb() -> Rational {
    Rational(1, SAMPLE_RATE as i32)
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

/// Encode `num_video_frames` luma planes (1920x1080 Y bytes, produced on demand
/// by `make_luma`) and `audio_pcm` (mono, 16-bit, 44.1 kHz) into an MP4 at
/// `output`. Frames are fed to the encoder as YUV420P directly with neutral
/// chroma, so there is no RGB->YUV swscale pass.
pub fn encode_to_file<F>(
    output: &Path,
    num_video_frames: usize,
    make_luma: F,
    audio_pcm: &[i16],
) -> Result<()>
where
    F: Fn(usize) -> Vec<u8>,
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

    // ---- audio stream + AAC encoder ----
    let (mut aenc, audio_idx, audio_frame_size) = {
        let acodec = encoder::find(codec::Id::AAC)
            .ok_or_else(|| anyhow!("AAC encoder not available"))?;
        let mut ost = octx.add_stream(acodec)?;
        let idx = ost.index();
        let mut enc = context_with_codec(acodec).encoder().audio()?;
        enc.set_rate(SAMPLE_RATE as i32);
        enc.set_channel_layout(ChannelLayout::MONO);
        enc.set_channels(1);
        enc.set_format(Sample::F32(SampleType::Planar)); // FLTP
        enc.set_bit_rate(320_000);
        enc.set_time_base(audio_enc_tb());
        if global_header {
            enc.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let enc = enc.open_as(acodec)?;
        ost.set_parameters(&enc);
        let fs = (enc.frame_size() as usize).max(1);
        (enc, idx, fs)
    };

    octx.write_header()?;
    let video_tb = octx.stream(video_idx).unwrap().time_base();
    let audio_tb = octx.stream(audio_idx).unwrap().time_base();

    let num_audio_frames = if audio_pcm.is_empty() {
        0
    } else {
        audio_pcm.len().div_ceil(audio_frame_size)
    };

    // Interleave video and audio in presentation-time order so the muxer's
    // interleaving queue stays small.
    let mut vi = 0usize;
    let mut ai = 0usize;
    loop {
        let v_time = if vi < num_video_frames {
            vi as f64 / FRAME_RATE as f64
        } else {
            f64::INFINITY
        };
        let a_time = if ai < num_audio_frames {
            (ai * audio_frame_size) as f64 / SAMPLE_RATE as f64
        } else {
            f64::INFINITY
        };
        if v_time.is_infinite() && a_time.is_infinite() {
            break;
        }

        if v_time <= a_time {
            let luma = make_luma(vi);
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
            vi += 1;
        } else {
            let start = ai * audio_frame_size;
            let end = (start + audio_frame_size).min(audio_pcm.len());
            let chunk = &audio_pcm[start..end];
            let mut af = frame::Audio::new(Sample::F32(SampleType::Planar), chunk.len(), ChannelLayout::MONO);
            af.set_rate(SAMPLE_RATE as u32);
            af.set_pts(Some(start as i64));
            {
                let plane = af.plane_mut::<f32>(0);
                for (i, &s) in chunk.iter().enumerate() {
                    plane[i] = s as f32 / 32768.0;
                }
            }
            aenc.send_frame(&af)?;
            drain_audio(&mut aenc, &mut octx, audio_idx, audio_tb)?;
            ai += 1;
        }
    }

    venc.send_eof()?;
    drain_video(&mut venc, &mut octx, video_idx, video_tb)?;
    aenc.send_eof()?;
    drain_audio(&mut aenc, &mut octx, audio_idx, audio_tb)?;

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

fn drain_audio(
    enc: &mut encoder::audio::Encoder,
    octx: &mut format::context::Output,
    stream_idx: usize,
    stream_tb: Rational,
) -> Result<()> {
    let mut pkt = Packet::empty();
    while enc.receive_packet(&mut pkt).is_ok() {
        pkt.set_stream(stream_idx);
        pkt.rescale_ts(audio_enc_tb(), stream_tb);
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

/// Decode the audio stream of `input` to normalized mono f32 samples (44.1 kHz).
/// Returns an empty vector when the file has no audio stream.
pub fn decode_audio_samples(input: &Path) -> Result<Vec<f32>> {
    ff_init()?;
    let mut ictx = format::input(&input)?;
    let audio_idx = match ictx.streams().best(media::Type::Audio) {
        Some(s) => s.index(),
        None => return Ok(Vec::new()),
    };
    let mut decoder = codec::context::Context::from_parameters(
        ictx.stream(audio_idx).unwrap().parameters(),
    )?
    .decoder()
    .audio()?;

    let mut out: Vec<f32> = Vec::new();
    for (s, packet) in ictx.packets() {
        if s.index() == audio_idx {
            decoder.send_packet(&packet)?;
            drain_decoded_audio(&mut decoder, &mut out)?;
        }
    }
    decoder.send_eof()?;
    drain_decoded_audio(&mut decoder, &mut out)?;
    Ok(out)
}

fn drain_decoded_audio(decoder: &mut ffmpeg::decoder::Audio, out: &mut Vec<f32>) -> Result<()> {
    let mut decoded = frame::Audio::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        let n = decoded.samples();
        match decoded.format() {
            Sample::F32(_) => out.extend_from_slice(&decoded.plane::<f32>(0)[..n]),
            Sample::I16(_) => {
                out.extend(decoded.plane::<i16>(0)[..n].iter().map(|&s| s as f32 / 32768.0));
            }
            other => anyhow::bail!("unexpected audio sample format: {other:?}"),
        }
    }
    Ok(())
}

/// Copy only the video stream of `src` into `dst` (drops audio). Used by tests
/// to verify the decoder works with no audio track.
pub fn remux_drop_audio(src: &Path, dst: &Path) -> Result<()> {
    ff_init()?;
    let mut ictx = format::input(&src)?;
    let mut octx = format::output(&dst)?;

    let video_idx = ictx
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| anyhow!("no video stream found"))?
        .index();

    let in_tb = {
        let ist = ictx.stream(video_idx).unwrap();
        let mut ost = octx.add_stream(encoder::find(codec::Id::None))?;
        ost.set_parameters(ist.parameters());
        unsafe {
            (*ost.parameters().as_mut_ptr()).codec_tag = 0;
        }
        ist.time_base()
    };

    octx.write_header()?;
    let out_tb = octx.stream(0).unwrap().time_base();

    for (s, mut packet) in ictx.packets() {
        if s.index() == video_idx {
            packet.rescale_ts(in_tb, out_tb);
            packet.set_position(-1);
            packet.set_stream(0);
            packet.write_interleaved(&mut octx)?;
        }
    }
    octx.write_trailer()?;
    Ok(())
}
