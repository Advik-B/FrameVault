"""
End-to-end integration tests: encode file → real MP4 → decode → verify SHA256.
Requires ffmpeg with libx264 and AAC in PATH.
"""

import hashlib
import io
import os
import random
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

import encode
import decode


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _encode(infile: str, video: str) -> str:
    """Run encode.encode(), capture stdout and return it."""
    buf = io.StringIO()
    with redirect_stdout(buf):
        encode.encode(infile, video)
    return buf.getvalue()


def _decode(video: str, outdir: str) -> str:
    """Run decode.decode(), capture stdout and return it."""
    buf = io.StringIO()
    with redirect_stdout(buf):
        decode.decode(video, outdir)
    return buf.getvalue()


class _RoundTripBase(unittest.TestCase):
    """Shared helpers for round-trip tests."""

    def _round_trip(self, data: bytes, filename: str = "test.bin") -> tuple[bytes, str, str]:
        """
        Encode data to MP4, decode back.
        Returns (recovered_bytes, encode_output, decode_output).
        """
        with tempfile.TemporaryDirectory() as tmpdir:
            infile = os.path.join(tmpdir, filename)
            video = os.path.join(tmpdir, "out.mp4")
            outdir = os.path.join(tmpdir, "rec")
            os.makedirs(outdir)
            Path(infile).write_bytes(data)

            try:
                enc_out = _encode(infile, video)
            except SystemExit as e:
                self.fail(f"encode() exited: {e}")

            self.assertTrue(os.path.exists(video), "Encoder produced no MP4")

            try:
                dec_out = _decode(video, outdir)
            except SystemExit as e:
                self.fail(f"decode() exited: {e}")

            recovered_path = os.path.join(outdir, filename)
            self.assertTrue(os.path.exists(recovered_path),
                            f"Recovered file missing: {recovered_path}")
            return Path(recovered_path).read_bytes(), enc_out, dec_out

    def _assert_round_trip(self, data: bytes, filename: str = "test.bin") -> tuple[str, str]:
        recovered, enc_out, dec_out = self._round_trip(data, filename)
        self.assertEqual(
            hashlib.sha256(recovered).hexdigest(),
            hashlib.sha256(data).hexdigest(),
            f"SHA256 mismatch: {filename} ({len(data)} bytes)",
        )
        self.assertEqual(recovered, data, f"Byte-level mismatch: {filename}")
        return enc_out, dec_out


# ---------------------------------------------------------------------------
# Size sweep — verifies encoding any file size and 16-bit index selection
# ---------------------------------------------------------------------------

class TestVariousSizes(_RoundTripBase):
    """Requirement 1 & 2: encode files of various sizes, 16-bit int for small."""

    def _check_16bit(self, enc_out: str, label: str):
        self.assertIn("Frame index:    16 bits", enc_out,
                      f"Expected 16-bit frame index for {label}")

    def test_1_byte(self):
        enc_out, _ = self._assert_round_trip(b"\xAB", "one.bin")
        self._check_16bit(enc_out, "1 byte")

    def test_22_bytes(self):
        # Theoretical threshold where audio can carry 100% of ECC
        enc_out, _ = self._assert_round_trip(bytes(range(22)), "threshold.bin")
        self._check_16bit(enc_out, "22 bytes")

    def test_100_bytes_random(self):
        data = bytes(random.Random(1).randint(0, 255) for _ in range(100))
        enc_out, _ = self._assert_round_trip(data, "rand100.bin")
        self._check_16bit(enc_out, "100 bytes")

    def test_1kb_random(self):
        data = bytes(random.Random(2).randint(0, 255) for _ in range(1024))
        enc_out, _ = self._assert_round_trip(data, "rand1k.bin")
        self._check_16bit(enc_out, "1 KB")

    def test_10kb_random(self):
        data = bytes(random.Random(3).randint(0, 255) for _ in range(10 * 1024))
        enc_out, _ = self._assert_round_trip(data, "rand10k.bin")
        self._check_16bit(enc_out, "10 KB")

    def test_100kb_random(self):
        # ~52 sec on a typical machine (39s encode + 12s decode)
        data = bytes(random.Random(4).randint(0, 255) for _ in range(100 * 1024))
        enc_out, _ = self._assert_round_trip(data, "rand100k.bin")
        self._check_16bit(enc_out, "100 KB")


# ---------------------------------------------------------------------------
# Data variety — verifies correctness across different data patterns
# ---------------------------------------------------------------------------

class TestDataVariety(_RoundTripBase):
    """All-zeros, all-0xFF, text, repeating patterns, binary."""

    def test_all_zeros(self):
        self._assert_round_trip(bytes(2048), "zeros.bin")

    def test_all_0xff(self):
        self._assert_round_trip(bytes([0xFF] * 2048), "ones.bin")

    def test_text_file(self):
        data = ("Hello FrameVault!\nThis is a test.\n" * 60).encode()
        self._assert_round_trip(data, "readme.txt")

    def test_repeating_256_cycle(self):
        self._assert_round_trip(bytes(range(256)) * 12, "pattern.bin")

    def test_binary_mixed(self):
        # Mix of zero runs, high-byte runs, and random
        data = bytes(1024) + bytes([0xFF] * 512) + bytes(
            random.Random(7).randint(0, 255) for _ in range(512)
        )
        self._assert_round_trip(data, "mixed.bin")


# ---------------------------------------------------------------------------
# Audio coverage — positions span the full ECC stream
# ---------------------------------------------------------------------------

class TestAudioCoverage(_RoundTripBase):
    """Requirement 3: distributed positions anchor at 0 and ecc_len-1."""

    def _assert_audio_coverage(self, data: bytes, filename: str):
        with tempfile.TemporaryDirectory() as tmpdir:
            infile = os.path.join(tmpdir, filename)
            Path(infile).write_bytes(data)
            payload, meta = encode.build_payload(infile)

        ecc = encode.ecc_encode(payload)
        ecc_len = len(ecc)
        _, data_bits = encode.frame_layout(encode.DEFAULT_FRAME_INDEX_BITS)
        num_frames = encode.frames_needed(ecc_len * 8, data_bits)
        duration = (num_frames + encode.METADATA_FRAMES) / encode.FRAME_RATE
        audio_count = min(ecc_len, int(duration * encode.BYTES_PER_SEC_AUDIO))

        positions = encode.compute_audio_positions(ecc_len, audio_count, encode.AUDIO_LAYOUT_DISTRIBUTED)

        if audio_count >= 2:
            self.assertEqual(int(positions[0]), 0,
                             f"First audio position not 0 for {filename}")
            self.assertEqual(int(positions[-1]), ecc_len - 1,
                             f"Last audio position not ecc_len-1 for {filename}")
            self.assertTrue((positions[1:] > positions[:-1]).all(),
                            "Audio positions not strictly monotone")

        # Encoder and decoder agree on positions
        dec_pos = decode.compute_audio_positions(ecc_len, audio_count, decode.AUDIO_LAYOUT_DISTRIBUTED)
        import numpy as np
        np.testing.assert_array_equal(positions, dec_pos,
            err_msg=f"Encoder/decoder position mismatch for {filename}")

    def test_1kb_audio_spans_full_ecc(self):
        data = bytes(random.Random(10).randint(0, 255) for _ in range(1024))
        self._assert_audio_coverage(data, "cov1k.bin")

    def test_10kb_audio_spans_full_ecc(self):
        data = bytes(random.Random(11).randint(0, 255) for _ in range(10 * 1024))
        self._assert_audio_coverage(data, "cov10k.bin")

    def test_audio_coverage_reported_in_encode_output(self):
        """Encoder output must show audio coverage and layout type."""
        data = bytes(random.Random(20).randint(0, 255) for _ in range(2 * 1024))
        with tempfile.TemporaryDirectory() as tmpdir:
            infile = os.path.join(tmpdir, "cov.bin")
            video = os.path.join(tmpdir, "out.mp4")
            Path(infile).write_bytes(data)
            try:
                enc_out = _encode(infile, video)
            except SystemExit as e:
                self.fail(f"encode failed: {e}")

        self.assertIn("Audio layout:   distributed", enc_out)
        self.assertIn("Audio covers:", enc_out)


# ---------------------------------------------------------------------------
# Audio independence — decoding works without any audio
# ---------------------------------------------------------------------------

class TestAudioIndependence(_RoundTripBase):
    """Requirement 4: video-only decode must succeed even with no audio."""

    def _strip_audio(self, src: str, dst: str):
        result = subprocess.run(
            ["ffmpeg", "-y", "-i", src, "-an", "-c:v", "copy", dst],
            capture_output=True,
        )
        self.assertEqual(result.returncode, 0,
                         f"ffmpeg audio strip failed:\n{result.stderr.decode()}")

    def _silence_audio(self, src: str, dst: str):
        result = subprocess.run(
            ["ffmpeg", "-y", "-i", src,
             "-f", "lavfi", "-i", "anullsrc=r=44100:cl=mono",
             "-c:v", "copy", "-c:a", "aac", "-shortest", dst],
            capture_output=True,
        )
        self.assertEqual(result.returncode, 0,
                         f"ffmpeg silence replacement failed:\n{result.stderr.decode()}")

    def _encode_and_modify(self, data: bytes, modifier, filename: str) -> bytes:
        with tempfile.TemporaryDirectory() as tmpdir:
            infile = os.path.join(tmpdir, filename)
            video_orig = os.path.join(tmpdir, "orig.mp4")
            video_mod = os.path.join(tmpdir, "mod.mp4")
            outdir = os.path.join(tmpdir, "rec")
            os.makedirs(outdir)
            Path(infile).write_bytes(data)

            try:
                _encode(infile, video_orig)
            except SystemExit as e:
                self.fail(f"encode failed: {e}")

            modifier(video_orig, video_mod)

            try:
                dec_out = _decode(video_mod, outdir)
            except SystemExit as e:
                self.fail(f"decode failed: {e}")

            # Must have used video-only strategy, not audio merge
            self.assertIn("RS decode succeeded on video stream", dec_out,
                          "Expected video-only RS strategy to succeed")
            self.assertNotIn("RS decode succeeded on merged stream", dec_out,
                             "Should not need audio merge for clean lossless video")

            return Path(os.path.join(outdir, filename)).read_bytes()

    def test_no_audio_track(self):
        """Decode MP4 with audio stream completely removed."""
        data = bytes(random.Random(30).randint(0, 255) for _ in range(2 * 1024))
        recovered = self._encode_and_modify(data, self._strip_audio, "no_audio.bin")
        self.assertEqual(recovered, data)

    def test_silent_audio_track(self):
        """Decode MP4 where audio was replaced with silence."""
        data = bytes(random.Random(31).randint(0, 255) for _ in range(2 * 1024))
        recovered = self._encode_and_modify(data, self._silence_audio, "silent.bin")
        self.assertEqual(recovered, data)

    def test_clean_video_uses_video_only_strategy(self):
        """Lossless local round-trip must succeed via Strategy A (video only)."""
        data = bytes(random.Random(32).randint(0, 255) for _ in range(3 * 1024))
        _, enc_out, dec_out = self._round_trip(data, "strategy_a.bin")
        self.assertIn("RS decode succeeded on video stream", dec_out,
                      "Expected Strategy A (video-only) to succeed for clean local video")


if __name__ == "__main__":
    unittest.main(verbosity=2)
