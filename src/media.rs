//! libav (ffmpeg-next) media pipeline — no subprocess.
//!
//! - [`encode_to_file`] muxes generated luma planes (lossless H.264 or H.265) into a
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

/// Video codec for the encoded MP4. Both are lossless locally; decode handles either
/// (the codec comes from the stream itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum VideoCodec {
    /// H.264 via libx264 (CRF 0). Fast; plays everywhere.
    #[default]
    H264,
    /// H.265/HEVC via libx265 (lossless mode). Slower to encode, and its files are no
    /// smaller than H.264's for FrameVault's block frames.
    H265,
}

/// x264/x265 speed preset (both encoders share the names). Output is lossless at every
/// preset, so this only trades encode time for file size, never data integrity.
///
/// Measured on 239 FrameVault frames (lossless x264 / x265): `ultrafast` 2.3 / 4.9 MB,
/// `fast` 262 / 333 KB, `medium` 185 / 219 KB, `veryslow` 178 / 189 KB. Encode time grows
/// from 1x (`ultrafast`) to about 1.5x (`fast`), 1.8x (`medium`) and 6.5x (`veryslow`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Preset {
    Ultrafast,
    Superfast,
    Veryfast,
    Faster,
    #[default]
    Fast,
    Medium,
    Slow,
    Slower,
    Veryslow,
}

impl Preset {
    pub fn as_str(self) -> &'static str {
        match self {
            Preset::Ultrafast => "ultrafast",
            Preset::Superfast => "superfast",
            Preset::Veryfast => "veryfast",
            Preset::Faster => "faster",
            Preset::Fast => "fast",
            Preset::Medium => "medium",
            Preset::Slow => "slow",
            Preset::Slower => "slower",
            Preset::Veryslow => "veryslow",
        }
    }
}

impl std::fmt::Display for Preset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl VideoCodec {
    fn encoder_name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "libx264",
            VideoCodec::H265 => "libx265",
        }
    }

    fn options(self, preset: Preset) -> Dictionary<'static> {
        let mut opts = Dictionary::new();
        opts.set("preset", preset.as_str());
        match self {
            VideoCodec::H264 => opts.set("crf", "0"), // lossless H.264
            // x265's CRF 0 is *not* lossless; its lossless mode is a separate switch.
            VideoCodec::H265 => opts.set("x265-params", "lossless=1:log-level=error"),
        }
        opts
    }
}

impl std::fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::H265 => "H.265",
        })
    }
}

/// Whether the linked FFmpeg has the encoder for `codec` (e.g. builds without libx265).
pub fn encoder_available(codec: VideoCodec) -> bool {
    ff_init().is_ok() && encoder::find_by_name(codec.encoder_name()).is_some()
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
/// `make_luma`) into a video-only MP4 at `output` with `video_codec` at `preset`. Frames
/// are fed to the encoder as YUV420P directly with neutral chroma, so there is no
/// RGB->YUV swscale pass.
///
/// `make_luma` is pulled with strictly increasing indices `0..num_video_frames`, so it
/// can drive a streaming generator; returning `Result` lets a mid-stream IO error from
/// that generator propagate out of the muxing loop.
pub fn encode_to_file<F>(
    output: &Path,
    num_video_frames: usize,
    video_codec: VideoCodec,
    preset: Preset,
    mut make_luma: F,
) -> Result<()>
where
    F: FnMut(usize) -> Result<Vec<u8>>,
{
    ff_init()?;
    let mut octx = format::output(&output)?;
    let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);

    // ---- video stream + encoder ----
    let (mut venc, video_idx) = {
        let vcodec = encoder::find_by_name(video_codec.encoder_name()).ok_or_else(|| {
            anyhow!(
                "{video_codec} encoder ({}) not available in this FFmpeg build",
                video_codec.encoder_name()
            )
        })?;
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
        let enc = enc.open_as_with(vcodec, video_codec.options(preset))?;
        ost.set_parameters(&enc);
        if video_codec == VideoCodec::H265 {
            // Tag HEVC as `hvc1` (not libav's default `hev1`) so Apple players and
            // browsers will play the MP4.
            unsafe {
                (*(*ost.as_mut_ptr()).codecpar).codec_tag = u32::from_le_bytes(*b"hvc1");
            }
        }
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
///
/// `on_frame` returning `Err` (e.g. a windowed RS-decode or disk I/O failure) aborts the
/// demux/decode loop and propagates out, mirroring `encode_to_file`'s `make_luma`.
pub fn decode_video_frames<F>(input: &Path, mut on_frame: F) -> Result<()>
where
    F: FnMut(&[u8]) -> Result<()>,
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
    F: FnMut(&[u8]) -> Result<()>,
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
        on_frame(buf)?;
    }
    Ok(())
}

