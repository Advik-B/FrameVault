//! QR metadata frames: generation (`qrcode`) and detection/decode (`rqrr`).
//! Replaces the Python `make_qr_frame` and the cv2/pyzbar `decode_qr_metadata`.
//!
//! Note: unlike the Python decoder (cv2 multi-scale + Otsu + pyzbar fallback),
//! this targets a clean, centered QR on a white frame. The decoder's payload-header
//! fallback still covers cases where QR detection fails.

use anyhow::{bail, Result};
use qrcode::{Color, EcLevel, QrCode};

use crate::constants::*;
use crate::frame::{BLACK_Y, WHITE_Y};
use crate::metadata::{parse_qr_meta, QrMeta};

/// Render the QR metadata `payload` as a centered QR on a white 1920x1080 luma
/// (Y) plane — the YUV420P equivalent of [`make_qr_frame`], avoiding RGB+swscale.
pub fn make_qr_luma(payload: &[u8]) -> Result<Vec<u8>> {
    let code = QrCode::with_error_correction_level(payload, EcLevel::Q)?;
    let w = code.width();
    let colors = code.to_colors();
    let total = w + 2 * QR_BORDER_MODULES;

    let max_w = FRAME_WIDTH - 2 * BLOCK_SIZE;
    let max_h = FRAME_HEIGHT - 2 * BLOCK_SIZE;
    let module_px = (max_w / total).min(max_h / total);
    if module_px < QR_MIN_MODULE_PX {
        bail!("QR modules too small ({module_px}px). Shorten metadata or reduce QR error correction.");
    }

    let side = total * module_px;
    let x0 = (FRAME_WIDTH - side) / 2;
    let y0 = (FRAME_HEIGHT - side) / 2;

    let mut luma = vec![WHITE_Y; FRAME_WIDTH * FRAME_HEIGHT];
    for mr in 0..total {
        for mc in 0..total {
            let dark = mr >= QR_BORDER_MODULES
                && mr < QR_BORDER_MODULES + w
                && mc >= QR_BORDER_MODULES
                && mc < QR_BORDER_MODULES + w
                && colors[(mr - QR_BORDER_MODULES) * w + (mc - QR_BORDER_MODULES)] == Color::Dark;
            if !dark {
                continue;
            }
            for py in 0..module_px {
                let off = (y0 + mr * module_px + py) * FRAME_WIDTH + x0 + mc * module_px;
                luma[off..off + module_px].fill(BLACK_Y);
            }
        }
    }
    Ok(luma)
}

/// Render the QR metadata `payload` as a centered QR code on a white 1920x1080
/// rgb24 frame. Errors if the modules would be smaller than `QR_MIN_MODULE_PX`.
pub fn make_qr_frame(payload: &[u8]) -> Result<Vec<u8>> {
    let code = QrCode::with_error_correction_level(payload, EcLevel::Q)?;
    let w = code.width();
    let colors = code.to_colors();
    let total = w + 2 * QR_BORDER_MODULES;

    let max_w = FRAME_WIDTH - 2 * BLOCK_SIZE;
    let max_h = FRAME_HEIGHT - 2 * BLOCK_SIZE;
    let module_px = (max_w / total).min(max_h / total);
    if module_px < QR_MIN_MODULE_PX {
        bail!("QR modules too small ({module_px}px). Shorten metadata or reduce QR error correction.");
    }

    let side = total * module_px;
    let x0 = (FRAME_WIDTH - side) / 2;
    let y0 = (FRAME_HEIGHT - side) / 2;

    let mut frame = vec![255u8; FRAME_WIDTH * FRAME_HEIGHT * 3]; // white background
    for mr in 0..total {
        for mc in 0..total {
            let dark = mr >= QR_BORDER_MODULES
                && mr < QR_BORDER_MODULES + w
                && mc >= QR_BORDER_MODULES
                && mc < QR_BORDER_MODULES + w
                && colors[(mr - QR_BORDER_MODULES) * w + (mc - QR_BORDER_MODULES)] == Color::Dark;
            if !dark {
                continue; // white module: background already 255
            }
            for py in 0..module_px {
                let y = y0 + mr * module_px + py;
                let off = (y * FRAME_WIDTH + x0 + mc * module_px) * 3;
                frame[off..off + module_px * 3].fill(0);
            }
        }
    }
    Ok(frame)
}

/// Detect and decode QR metadata from a 1920x1080 rgb24 frame (sampling the R
/// channel as luma). Returns `None` if no QR with the required fields is found.
pub fn decode_qr_metadata(frame: &[u8]) -> Option<QrMeta> {
    let mut img = rqrr::PreparedImage::prepare_from_greyscale(FRAME_WIDTH, FRAME_HEIGHT, |x, y| {
        frame[(y * FRAME_WIDTH + x) * 3]
    });
    for grid in img.detect_grids() {
        if let Ok((_meta, content)) = grid.decode() {
            if let Some(meta) = parse_qr_meta(&content) {
                return Some(meta);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{build_qr_metadata, Meta};

    #[test]
    fn qr_frame_round_trip() {
        let sha = "a".repeat(64);
        let meta = Meta {
            v: ENCODING_VERSION,
            filename: "round_trip.bin".into(),
            size: 123_456,
            sha256: sha.clone(),
        };
        let payload = build_qr_metadata(&meta, 4096, 72, 16);
        let frame = make_qr_frame(&payload).expect("render qr");
        assert_eq!(frame.len(), FRAME_WIDTH * FRAME_HEIGHT * 3);

        let decoded = decode_qr_metadata(&frame).expect("decode qr");
        assert_eq!(decoded.filename.as_deref(), Some("round_trip.bin"));
        assert_eq!(decoded.size, Some(123_456));
        assert_eq!(decoded.sha256.as_deref(), Some(sha.as_str()));
        assert_eq!(decoded.ecc_bytes, Some(4096));
        assert_eq!(decoded.frames, Some(72));
        assert_eq!(decoded.index_bits, Some(16));
    }
}
