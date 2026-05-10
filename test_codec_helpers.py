import unittest

import encode
import decode


class AudioLayoutTests(unittest.TestCase):
    def test_distributed_audio_positions_span_stream(self):
        positions = encode.compute_audio_positions(1000, 25, encode.AUDIO_LAYOUT_DISTRIBUTED)
        self.assertEqual(len(positions), 25)
        self.assertGreater(positions[0], 0)
        self.assertLess(positions[0], 100)
        self.assertGreater(positions[-1], 900)
        self.assertTrue((positions[1:] > positions[:-1]).all())

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


if __name__ == "__main__":
    unittest.main()
