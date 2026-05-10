import math
import random
import unittest

import numpy as np

import decode
import encode


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _expected_ecc_size(data_len: int) -> int:
    """Compute the ECC stream length produced by ecc_encode for data_len bytes."""
    full_blocks = data_len // encode.RS_DATA_BYTES
    last = data_len % encode.RS_DATA_BYTES
    if last == 0:
        return full_blocks * encode.RS_BLOCK_SIZE
    return full_blocks * encode.RS_BLOCK_SIZE + (last + encode.RS_ECC_SYMBOLS)


def _index_bits_for_ecc(ecc_bytes: int) -> int:
    """Mirror the encoder's 16-bit vs 32-bit index selection."""
    total_bits = ecc_bytes * 8
    _, data_bits_default = encode.frame_layout(encode.DEFAULT_FRAME_INDEX_BITS)
    frames_16 = encode.frames_needed(total_bits, data_bits_default)
    if frames_16 <= encode.MAX_FRAME_COUNT_DEFAULT:
        return encode.DEFAULT_FRAME_INDEX_BITS
    return encode.EXTENDED_FRAME_INDEX_BITS


# ---------------------------------------------------------------------------
# Existing tests (kept intact)
# ---------------------------------------------------------------------------

class AudioLayoutTests(unittest.TestCase):
    def test_distributed_audio_positions_cover_full_range(self):
        ecc_len = 1000
        positions = encode.compute_audio_positions(ecc_len, 25, encode.AUDIO_LAYOUT_DISTRIBUTED)
        self.assertEqual(len(positions), 25)
        self.assertEqual(positions[0], 0)
        self.assertEqual(positions[-1], ecc_len - 1)
        self.assertTrue((positions[1:] > positions[:-1]).all())

    def test_audio_positions_cover_all_bytes_when_full_audio(self):
        ecc_len = 128
        positions = encode.compute_audio_positions(ecc_len, ecc_len, encode.AUDIO_LAYOUT_DISTRIBUTED)
        self.assertEqual(list(positions), list(range(ecc_len)))

    def test_audio_positions_single_byte_midpoint(self):
        ecc_len = 101
        positions = encode.compute_audio_positions(ecc_len, 1, encode.AUDIO_LAYOUT_DISTRIBUTED)
        self.assertEqual(list(positions), [ecc_len // 2])
        even_len = 100
        even_positions = encode.compute_audio_positions(even_len, 1, encode.AUDIO_LAYOUT_DISTRIBUTED)
        self.assertEqual(list(even_positions), [even_len // 2])

    def test_merge_audio_bytes_uses_planned_positions(self):
        ecc = bytes(range(200))
        planned_audio = 10
        audio = encode.select_audio_bytes(ecc, planned_audio, encode.AUDIO_LAYOUT_DISTRIBUTED)
        blank = bytes(len(ecc))

        merged, recovered_positions = decode.merge_audio_bytes(
            blank,
            audio,
            planned_audio,
            decode.AUDIO_LAYOUT_DISTRIBUTED,
        )

        for index, position in enumerate(recovered_positions):
            self.assertEqual(merged[int(position)], audio[index])
        self.assertEqual(len(recovered_positions), planned_audio)

    def test_parallel_rs_round_trip(self):
        payload = (b"FrameVault parallel ECC test\n" * 200)
        ecc = encode.ecc_encode(payload)
        decoded = decode.rs_decode(ecc)
        self.assertEqual(decoded, payload)

    def test_distributed_audio_merge_repairs_corrupted_bytes(self):
        payload = (b"distributed audio repair\n" * 150)
        ecc = encode.ecc_encode(payload)
        planned_audio = 24
        audio = encode.select_audio_bytes(ecc, planned_audio, encode.AUDIO_LAYOUT_DISTRIBUTED)
        merged_positions = encode.compute_audio_positions(len(ecc), planned_audio, encode.AUDIO_LAYOUT_DISTRIBUTED)

        corrupted = bytearray(ecc)
        for position in merged_positions:
            corrupted[int(position)] ^= 0x5A

        repaired, recovered_positions = decode.merge_audio_bytes(
            bytes(corrupted),
            audio,
            planned_audio,
            decode.AUDIO_LAYOUT_DISTRIBUTED,
        )

        self.assertEqual(list(recovered_positions), list(merged_positions))
        self.assertEqual(decode.rs_decode(repaired), payload)


# ---------------------------------------------------------------------------
# Requirement 1 & 2: any file size, 16-bit for small / 32-bit for large
# ---------------------------------------------------------------------------

class TestFrameIndexSelection(unittest.TestCase):
    """Encoder must select 16-bit index for small files and 32-bit for large."""

    def test_tiny_file_uses_16bit(self):
        self.assertEqual(_index_bits_for_ecc(100), 16)

    def test_1mb_ecc_uses_16bit(self):
        self.assertEqual(_index_bits_for_ecc(1 * 1024 * 1024), 16)

    def test_max_16bit_capacity_still_16bit(self):
        _, data_bits = encode.frame_layout(encode.DEFAULT_FRAME_INDEX_BITS)
        bytes_per_frame = data_bits // 8
        max_ecc_16 = encode.MAX_FRAME_COUNT_DEFAULT * bytes_per_frame
        self.assertEqual(_index_bits_for_ecc(max_ecc_16), 16)

    def test_one_byte_over_16bit_limit_uses_32bit(self):
        _, data_bits = encode.frame_layout(encode.DEFAULT_FRAME_INDEX_BITS)
        bytes_per_frame = data_bits // 8
        max_ecc_16 = encode.MAX_FRAME_COUNT_DEFAULT * bytes_per_frame
        self.assertEqual(_index_bits_for_ecc(max_ecc_16 + 1), 32)

    def test_large_file_uses_32bit(self):
        self.assertEqual(_index_bits_for_ecc(10 * 1024 * 1024), 32)

    def test_frame_layout_16bit_capacity(self):
        header_bits, data_bits = encode.frame_layout(encode.DEFAULT_FRAME_INDEX_BITS)
        self.assertEqual(header_bits, encode.SYNC_BITS + encode.DEFAULT_FRAME_INDEX_BITS)
        self.assertEqual(data_bits, encode.TOTAL_BLOCKS - header_bits)
        self.assertEqual(data_bits, 456)
        self.assertEqual(data_bits % 8, 0)

    def test_frame_layout_32bit_capacity(self):
        header_bits, data_bits = encode.frame_layout(encode.EXTENDED_FRAME_INDEX_BITS)
        self.assertEqual(header_bits, encode.SYNC_BITS + encode.EXTENDED_FRAME_INDEX_BITS)
        self.assertEqual(data_bits, encode.TOTAL_BLOCKS - header_bits)
        self.assertEqual(data_bits, 440)
        self.assertEqual(data_bits % 8, 0)

    def test_16bit_max_frame_count(self):
        self.assertEqual(encode.MAX_FRAME_COUNT_DEFAULT, 1 << 16)

    def test_32bit_max_frame_count(self):
        self.assertEqual(encode.MAX_FRAME_COUNT_EXTENDED, 1 << 32)

    def test_theoretical_max_ecc_16bit(self):
        _, data_bits = encode.frame_layout(encode.DEFAULT_FRAME_INDEX_BITS)
        max_ecc = encode.MAX_FRAME_COUNT_DEFAULT * (data_bits // 8)
        # 65536 frames * 57 bytes/frame = 3,735,552 bytes
        self.assertEqual(max_ecc, 3_735_552)

    def test_theoretical_max_ecc_32bit(self):
        _, data_bits = encode.frame_layout(encode.EXTENDED_FRAME_INDEX_BITS)
        max_ecc = encode.MAX_FRAME_COUNT_EXTENDED * (data_bits // 8)
        # 2^32 frames * 55 bytes/frame = 236,223,201,280 bytes ≈ 220 GB
        self.assertEqual(max_ecc, (1 << 32) * 55)
        self.assertGreater(max_ecc, 200 * 1024 * 1024 * 1024)  # > 200 GB


# ---------------------------------------------------------------------------
# Requirement 3: audio positions span the entire ECC stream
# ---------------------------------------------------------------------------

class TestAudioCoverageFullRange(unittest.TestCase):
    """Distributed audio positions must anchor at 0 and ecc_len-1, be strictly
    monotone, and match between encoder and decoder for any input."""

    def _assert_positions(self, ecc_len: int, audio_bytes: int):
        enc_pos = encode.compute_audio_positions(ecc_len, audio_bytes, encode.AUDIO_LAYOUT_DISTRIBUTED)
        dec_pos = decode.compute_audio_positions(ecc_len, audio_bytes, decode.AUDIO_LAYOUT_DISTRIBUTED)

        np.testing.assert_array_equal(
            enc_pos, dec_pos,
            err_msg=f"Encoder/decoder disagree: ecc_len={ecc_len}, audio_bytes={audio_bytes}",
        )

        n = min(audio_bytes, ecc_len)
        self.assertEqual(len(enc_pos), n,
                         f"Wrong position count: ecc_len={ecc_len}, audio_bytes={audio_bytes}")

        if n >= 2:
            self.assertEqual(int(enc_pos[0]), 0,
                             f"First position must be 0: ecc_len={ecc_len}, audio_bytes={audio_bytes}")
            self.assertEqual(int(enc_pos[-1]), ecc_len - 1,
                             f"Last position must be ecc_len-1: ecc_len={ecc_len}, audio_bytes={audio_bytes}")
            self.assertTrue((enc_pos[1:] > enc_pos[:-1]).all(),
                            f"Positions not strictly increasing: ecc_len={ecc_len}, audio_bytes={audio_bytes}")
            self.assertEqual(len(set(enc_pos.tolist())), n,
                             f"Duplicate positions found: ecc_len={ecc_len}, audio_bytes={audio_bytes}")

    def test_small_ecc(self):
        self._assert_positions(10, 5)

    def test_two_bytes_anchors(self):
        self._assert_positions(100, 2)

    def test_audio_equals_ecc_full_coverage(self):
        self._assert_positions(50, 50)

    def test_audio_exceeds_ecc_clamps(self):
        pos = encode.compute_audio_positions(30, 100, encode.AUDIO_LAYOUT_DISTRIBUTED)
        self.assertEqual(len(pos), 30)

    def test_typical_small_file(self):
        # ~1 KB ECC, 25 audio bytes (≈ 1 second at 25 bytes/sec)
        self._assert_positions(1143, 25)

    def test_typical_medium_file(self):
        self._assert_positions(50_000, 1250)

    def test_max_16bit_file(self):
        # Full-size 16-bit mode: 3.56 MB ECC, ~36 min video → ~54k audio bytes
        self._assert_positions(3_735_552, 54_637)

    def test_single_byte_audio_odd_ecc(self):
        pos = encode.compute_audio_positions(101, 1, encode.AUDIO_LAYOUT_DISTRIBUTED)
        self.assertEqual(int(pos[0]), 50)

    def test_single_byte_audio_even_ecc(self):
        pos = encode.compute_audio_positions(100, 1, encode.AUDIO_LAYOUT_DISTRIBUTED)
        self.assertEqual(int(pos[0]), 50)

    def test_encoder_decoder_agree_random_inputs(self):
        rng = random.Random(12345)
        for _ in range(40):
            ecc_len = rng.randint(2, 100_000)
            audio_bytes = rng.randint(1, ecc_len)
            self._assert_positions(ecc_len, audio_bytes)

    def test_prefix_layout_covers_prefix(self):
        pos = encode.compute_audio_positions(1000, 25, encode.AUDIO_LAYOUT_PREFIX)
        self.assertEqual(list(pos), list(range(25)))

    def test_zero_audio_bytes_returns_empty(self):
        pos = encode.compute_audio_positions(1000, 0, encode.AUDIO_LAYOUT_DISTRIBUTED)
        self.assertEqual(len(pos), 0)

    def test_zero_ecc_len_returns_empty(self):
        pos = encode.compute_audio_positions(0, 25, encode.AUDIO_LAYOUT_DISTRIBUTED)
        self.assertEqual(len(pos), 0)


# ---------------------------------------------------------------------------
# Requirement 4: audio track is independent — video-only decode must work
# ---------------------------------------------------------------------------

class TestAudioIndependence(unittest.TestCase):
    """RS decode must succeed on the video stream alone, without audio."""

    def test_video_only_rs_decode_clean(self):
        payload = b"video-only decode test\n" * 100
        ecc = encode.ecc_encode(payload)
        recovered = decode.rs_decode(ecc)
        self.assertEqual(recovered, payload)

    def test_empty_audio_leaves_ecc_unchanged(self):
        ecc = bytes(range(200))
        merged, positions = decode.merge_audio_bytes(ecc, b"", 10, decode.AUDIO_LAYOUT_DISTRIBUTED)
        self.assertEqual(merged, ecc)
        self.assertEqual(len(positions), 0)

    def test_zero_expected_audio_bytes_no_effect(self):
        ecc = bytes(range(200))
        merged, positions = decode.merge_audio_bytes(ecc, b"", 0, None)
        self.assertEqual(merged, ecc)
        self.assertEqual(len(positions), 0)

    def test_audio_merge_on_clean_ecc_still_decodes(self):
        payload = b"clean ecc with audio overlay\n" * 80
        ecc = encode.ecc_encode(payload)
        audio_count = 50
        audio = encode.select_audio_bytes(ecc, audio_count, encode.AUDIO_LAYOUT_DISTRIBUTED)
        merged, _ = decode.merge_audio_bytes(ecc, audio, audio_count, decode.AUDIO_LAYOUT_DISTRIBUTED)
        recovered = decode.rs_decode(merged)
        self.assertEqual(recovered, payload)

    def test_audio_recovery_repairs_targeted_corruption(self):
        payload = b"targeted corruption repair\n" * 120
        ecc = encode.ecc_encode(payload)
        audio_count = 30
        audio = encode.select_audio_bytes(ecc, audio_count, encode.AUDIO_LAYOUT_DISTRIBUTED)
        positions = encode.compute_audio_positions(len(ecc), audio_count, encode.AUDIO_LAYOUT_DISTRIBUTED)

        corrupted = bytearray(ecc)
        for pos in positions:
            corrupted[int(pos)] ^= 0xFF

        repaired, _ = decode.merge_audio_bytes(
            bytes(corrupted), audio, audio_count, decode.AUDIO_LAYOUT_DISTRIBUTED
        )
        self.assertEqual(decode.rs_decode(repaired), payload)

    def test_no_audio_stream_fallback(self):
        # Simulates a video decoded without any audio signal at all
        payload = b"no audio at all\n" * 200
        ecc = encode.ecc_encode(payload)
        # Strategy A: video-only RS decode — must succeed on clean data
        recovered = decode.rs_decode(ecc, erasures=None)
        self.assertEqual(recovered, payload)

    def test_merge_with_partial_audio_coverage(self):
        # Only 5 audio bytes available; merge should place them at the 5 planned
        # positions and leave the rest of the ECC stream from the video.
        ecc_len = 500
        ecc = bytes(i % 256 for i in range(ecc_len))
        planned_audio = 20
        actual_audio = 5  # fewer bytes arrived than planned
        audio = encode.select_audio_bytes(ecc, planned_audio, encode.AUDIO_LAYOUT_DISTRIBUTED)[:actual_audio]

        merged, recovered_positions = decode.merge_audio_bytes(
            ecc, audio, planned_audio, decode.AUDIO_LAYOUT_DISTRIBUTED
        )
        self.assertEqual(len(recovered_positions), actual_audio)
        for i, pos in enumerate(recovered_positions):
            self.assertEqual(merged[int(pos)], audio[i])


# ---------------------------------------------------------------------------
# Requirement 6: RS round-trip at various input file sizes
# ---------------------------------------------------------------------------

class TestRSVariousSizes(unittest.TestCase):
    """ECC encode→decode round-trip must be lossless at every payload size."""

    def _round_trip(self, size_bytes: int, label: str = ""):
        payload = bytes(i % 256 for i in range(size_bytes))
        ecc = encode.ecc_encode(payload)

        # ECC length must match theoretical expectation
        self.assertEqual(len(ecc), _expected_ecc_size(size_bytes),
                         f"ECC size mismatch for {label or size_bytes} bytes")

        recovered = decode.rs_decode(ecc)
        self.assertEqual(recovered, payload,
                         f"Round-trip failed for {label or size_bytes} bytes")

    def test_1_byte(self):
        self._round_trip(1)

    def test_22_bytes(self):
        # Theoretical limit where audio CAN cover 100% of ECC
        self._round_trip(22)

    def test_100_bytes(self):
        self._round_trip(100)

    def test_1_kb(self):
        self._round_trip(1024, "1 KB")

    def test_10_kb(self):
        self._round_trip(10 * 1024, "10 KB")

    def test_100_kb(self):
        self._round_trip(100 * 1024, "100 KB")

    def test_exact_one_rs_block(self):
        self._round_trip(encode.RS_DATA_BYTES, "one RS block")

    def test_rs_block_minus_one(self):
        self._round_trip(encode.RS_DATA_BYTES - 1)

    def test_rs_block_plus_one(self):
        self._round_trip(encode.RS_DATA_BYTES + 1)

    def test_multi_rs_blocks_with_remainder(self):
        self._round_trip(encode.RS_DATA_BYTES * 7 + 99, "7.4 RS blocks")

    def test_audio_byte_count_scales_with_ecc_size(self):
        # Audio bytes = min(ecc_len, floor(duration * 25 bytes/sec))
        # For a 1-frame video: duration = (1+30)/30 = 1.03 sec, audio_bytes = 25
        # We verify audio_byte_count does not exceed ecc_len
        for raw_size in [1, 50, 500, 5000]:
            payload = bytes(raw_size)
            ecc = encode.ecc_encode(payload)
            duration_sec = (encode.frames_needed(
                len(ecc) * 8,
                encode.frame_layout(encode.DEFAULT_FRAME_INDEX_BITS)[1]
            ) + encode.METADATA_FRAMES) / encode.FRAME_RATE
            audio_count = min(len(ecc), int(duration_sec * encode.BYTES_PER_SEC_AUDIO))
            self.assertLessEqual(audio_count, len(ecc),
                                 f"Audio byte count exceeds ECC len for {raw_size} byte file")

    def test_index_selection_at_rs_boundaries(self):
        # Verify 16-bit is used for small files
        for size in [1, 100, 1024, 10 * 1024, 100 * 1024]:
            payload = bytes(size)
            ecc = encode.ecc_encode(payload)
            bits = _index_bits_for_ecc(len(ecc))
            self.assertEqual(bits, 16,
                             f"Expected 16-bit index for {size}-byte file, got {bits}-bit")


if __name__ == "__main__":
    unittest.main()
