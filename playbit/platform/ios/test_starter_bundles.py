"""Run after build.sh; check the compiled guest selection, not just bundle labels."""
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[3]


class StarterBundleTests(unittest.TestCase):
    def test_contributor_kit_contains_three_distinct_compiled_guests(self):
        # A stale src/lib.rs copy must not silently turn all three fixtures into TODO.
        markers = {"todo": b"TODO", "ui-gallery": b"Regular button", "micro-kart": b"Micro Kart"}
        for guest, marker in markers.items():
            with self.subTest(guest=guest):
                data = (ROOT / "target/debug" / (guest + ".pbapp")).read_bytes()
                self.assertEqual(data[:8], b"PBAPP\0\0\0")
                count = int.from_bytes(data[12:16], "little")
                cursor = 32
                entries = {}
                for _ in range(count):
                    size = int.from_bytes(data[cursor:cursor+4], "little")
                    offset = int.from_bytes(data[cursor+8:cursor+16], "little")
                    length = int.from_bytes(data[cursor+16:cursor+24], "little")
                    path = data[cursor+24:cursor+24+size].decode("utf-8")
                    entries[path] = data[offset:offset+length]
                    cursor = (cursor + 24 + size + 7) & ~7
                self.assertEqual(json.loads(entries["app/manifest.json"])["name"], guest)
                initializers = json.loads(entries["app/volumes.json"])
                self.assertIsInstance(initializers, dict)
                if guest == "todo":
                    self.assertEqual(initializers["session"], json.loads(
                        (ROOT / "guests/todo/playbit-volumes.json").read_bytes())["session"])
                wasm = entries["app/main.wasm"]
                self.assertTrue(wasm.startswith(b"\0asm\x01\0\0\0"))
                self.assertIn(marker, wasm)
                for other, other_marker in markers.items():
                    if other != guest:
                        self.assertNotIn(other_marker, wasm)
                if guest == "ui-gallery":
                    self.assertIn((ROOT / "guests/ui-gallery/assets/scroll-photo.jpg").read_bytes(), wasm)


if __name__ == "__main__":
    unittest.main()
