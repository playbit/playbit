"""Keep developer-tool IDs identical to the native and browser boundaries."""
import json
from pathlib import Path
import unittest
from identifier import encode, decode


class Identifiers(unittest.TestCase):
    def test_shared_vectors_and_canonical_widths(self):
        path = Path(__file__).resolve().parents[1] / 'playbit/tests/fixtures/identifiers.json'
        for vector in json.loads(path.read_text()):
            raw = bytes(vector['bytes'])
            self.assertEqual(encode(raw), vector['text'])
            self.assertEqual(decode(vector['text'], len(raw)), raw)
            with self.assertRaises((ValueError, OverflowError)):
                decode('1' + vector['text'], len(raw))
