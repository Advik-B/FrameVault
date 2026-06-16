//! Audio channel: 4-FSK modulation/demodulation and ECC byte placement.
//! Mirrors the audio functions in encode.py / decode.py.

use realfft::RealFftPlanner;

use crate::constants::*;

/// ECC byte positions covered by the audio side-channel.
///
/// `layout = Some("distributed")` spreads `count` positions evenly across the
/// whole stream (anchored at `0` and `ecc_len-1`); `None` or `Some("prefix")`
/// covers the prefix `0..count`.
pub fn compute_audio_positions(
    ecc_len: usize,
    audio_byte_count: usize,
    layout: Option<&str>,
) -> Vec<usize> {
    if ecc_len == 0 || audio_byte_count == 0 {
        return Vec::new();
    }
    let count = audio_byte_count.min(ecc_len);
    let distributed = layout == Some(AUDIO_LAYOUT_DISTRIBUTED);
    if !distributed || count >= ecc_len {
        return (0..count).collect();
    }
    if count == 1 {
        return vec![ecc_len / 2];
    }
    // Evenly spaced over [0, ecc_len-1], rounding ties to even (matches numpy.round).
    let span = (ecc_len - 1) as f64;
    let denom = (count - 1) as f64;
    (0..count)
        .map(|i| ((i as f64) * span / denom).round_ties_even() as usize)
        .collect()
}

/// Select the ECC bytes that ride the audio channel.
pub fn select_audio_bytes(
    ecc_data: &[u8],
    audio_byte_count: usize,
    layout: Option<&str>,
) -> Vec<u8> {
    compute_audio_positions(ecc_data.len(), audio_byte_count, layout)
        .into_iter()
        .map(|p| ecc_data[p])
        .collect()
}

/// Overlay recovered audio bytes onto the video ECC stream at their planned
/// positions. Returns `(merged_stream, positions_written)`.
pub fn merge_audio_bytes(
    ecc_video: &[u8],
    audio_bytes: &[u8],
    expected_audio_bytes: usize,
    layout: Option<&str>,
) -> (Vec<u8>, Vec<usize>) {
    if audio_bytes.is_empty() || ecc_video.is_empty() {
        return (ecc_video.to_vec(), Vec::new());
    }
    let planned = compute_audio_positions(ecc_video.len(), expected_audio_bytes, layout);
    if planned.is_empty() {
        return (ecc_video.to_vec(), Vec::new());
    }
    let take = audio_bytes.len().min(planned.len());
    let recovered: Vec<usize> = planned[..take].to_vec();
    let mut merged = ecc_video.to_vec();
    for (i, &pos) in recovered.iter().enumerate() {
        merged[pos] = audio_bytes[i];
    }
    (merged, recovered)
}

/// Generate 4-FSK PCM (mono, 16-bit, 44.1 kHz) for `data`, padded with silence to
/// `duration_sec`. Each 2-bit symbol is one 441-sample tone burst (phase reset per
/// symbol), normalized to 0.9 peak.
pub fn make_audio_pcm(data: &[u8], duration_sec: f64) -> Vec<i16> {
    let total_samples = (SAMPLE_RATE as f64 * duration_sec) as usize;
    let max_symbols = total_samples / SAMPLES_PER_SYMBOL;

    // Unpack bits (MSB first), group into 2-bit symbols (values 0..3).
    let mut bits: Vec<u8> = Vec::with_capacity(data.len() * 8);
    for &byte in data {
        for k in (0..8).rev() {
            bits.push((byte >> k) & 1);
        }
    }
    let n_pairs = bits.len() / BITS_PER_SYMBOL;
    let mut symbols: Vec<usize> = (0..n_pairs)
        .map(|p| (bits[2 * p] as usize) * 2 + bits[2 * p + 1] as usize)
        .collect();
    if symbols.len() > max_symbols {
        symbols.truncate(max_symbols);
    }

    let mut audio = vec![0f64; total_samples];
    for (s_idx, &sym) in symbols.iter().enumerate() {
        let freq = FSK_FREQS[sym];
        let base = s_idx * SAMPLES_PER_SYMBOL;
        for s in 0..SAMPLES_PER_SYMBOL {
            audio[base + s] =
                (std::f64::consts::TAU * freq * (s as f64) / (SAMPLE_RATE as f64)).sin();
        }
    }

    let peak = audio.iter().fold(0f64, |m, &v| m.max(v.abs()));
    if peak > 0.0 {
        let scale = 0.9 / peak;
        for v in audio.iter_mut() {
            *v *= scale;
        }
    }
    audio.iter().map(|&v| (v * 32767.0) as i16).collect()
}

/// Convert signed 16-bit PCM to normalized f32 samples (matches the Python
/// decoder's `i16 / 32768` scaling).
pub fn pcm_i16_to_f32(pcm: &[i16]) -> Vec<f32> {
    pcm.iter().map(|&s| s as f32 / 32768.0).collect()
}

/// Demodulate 4-FSK PCM (f32 samples in [-1, 1]) back into bytes. Each
/// 441-sample window is Hann-windowed, real-FFT'd, and the dominant of the four
/// target frequency bins selects the 2-bit symbol.
pub fn decode_fsk(samples: &[f32]) -> Vec<u8> {
    let n_symbols = samples.len() / SAMPLES_PER_SYMBOL;
    if n_symbols == 0 {
        return Vec::new();
    }

    let bin_width = SAMPLE_RATE as f64 / SAMPLES_PER_SYMBOL as f64; // 100 Hz
    let target_bins: Vec<usize> = FSK_FREQS
        .iter()
        .map(|&f| (f / bin_width).round() as usize)
        .collect();

    let m = SAMPLES_PER_SYMBOL;
    // numpy hanning(M) = 0.5 - 0.5*cos(2*pi*n/(M-1))
    let hann: Vec<f32> = (0..m)
        .map(|n| (0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / (m as f64 - 1.0)).cos()) as f32)
        .collect();

    let mut planner = RealFftPlanner::<f32>::new();
    let r2c = planner.plan_fft_forward(m);
    let mut input = r2c.make_input_vec();
    let mut spectrum = r2c.make_output_vec();

    let mut bits: Vec<u8> = Vec::with_capacity(n_symbols * BITS_PER_SYMBOL);
    for sidx in 0..n_symbols {
        let start = sidx * m;
        for i in 0..m {
            input[i] = samples[start + i] * hann[i];
        }
        r2c.process(&mut input, &mut spectrum).expect("rfft");

        let mut best_sym = 0usize;
        let mut best_mag = -1f32;
        for (sym, &bin) in target_bins.iter().enumerate() {
            let mag = spectrum[bin].norm();
            if mag > best_mag {
                best_mag = mag;
                best_sym = sym;
            }
        }
        bits.push(((best_sym >> 1) & 1) as u8);
        bits.push((best_sym & 1) as u8);
    }

    // Pack bits (MSB first) into bytes.
    let n_bytes = bits.len() / 8;
    (0..n_bytes)
        .map(|b| {
            (0..8).fold(0u8, |acc, k| (acc << 1) | bits[b * 8 + k])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fsk_round_trip_recovers_data_prefix() {
        let data: Vec<u8> = (0..40u8).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
        let needed_symbols = data.len() * 4; // 8 bits/byte / 2 bits/symbol
        let duration =
            (needed_symbols * SAMPLES_PER_SYMBOL + SAMPLES_PER_SYMBOL) as f64 / SAMPLE_RATE as f64;
        let pcm = make_audio_pcm(&data, duration);
        let decoded = decode_fsk(&pcm_i16_to_f32(&pcm));
        assert!(decoded.len() >= data.len());
        assert_eq!(&decoded[..data.len()], &data[..]);
    }

    #[test]
    fn distributed_positions_anchor_and_increase() {
        let pos = compute_audio_positions(1000, 25, Some(AUDIO_LAYOUT_DISTRIBUTED));
        assert_eq!(pos.len(), 25);
        assert_eq!(pos[0], 0);
        assert_eq!(*pos.last().unwrap(), 999);
        assert!(pos.windows(2).all(|w| w[1] > w[0]));
    }

    #[test]
    fn position_edge_cases() {
        assert_eq!(compute_audio_positions(101, 1, Some(AUDIO_LAYOUT_DISTRIBUTED)), vec![50]);
        assert_eq!(compute_audio_positions(100, 1, Some(AUDIO_LAYOUT_DISTRIBUTED)), vec![50]);
        assert_eq!(
            compute_audio_positions(1000, 25, Some(AUDIO_LAYOUT_PREFIX)),
            (0..25).collect::<Vec<_>>()
        );
        assert!(compute_audio_positions(0, 25, Some(AUDIO_LAYOUT_DISTRIBUTED)).is_empty());
        assert!(compute_audio_positions(1000, 0, Some(AUDIO_LAYOUT_DISTRIBUTED)).is_empty());
        assert_eq!(
            compute_audio_positions(128, 128, Some(AUDIO_LAYOUT_DISTRIBUTED)),
            (0..128).collect::<Vec<_>>()
        );
    }

    #[test]
    fn merge_writes_audio_at_planned_positions() {
        let ecc: Vec<u8> = (0..200u8).collect();
        let planned = 10;
        let audio = select_audio_bytes(&ecc, planned, Some(AUDIO_LAYOUT_DISTRIBUTED));
        let blank = vec![0u8; ecc.len()];
        let (merged, positions) =
            merge_audio_bytes(&blank, &audio, planned, Some(AUDIO_LAYOUT_DISTRIBUTED));
        assert_eq!(positions.len(), planned);
        for (i, &p) in positions.iter().enumerate() {
            assert_eq!(merged[p], audio[i]);
        }
    }
}
