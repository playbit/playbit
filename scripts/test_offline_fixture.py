"""Template consumers sign private copies through explicit fixture authority."""
import json
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace

from identifier import encode
import offline_fixture


class TemplateSigningTests(unittest.TestCase):
    def test_signing_uses_only_the_private_copy_and_selected_profile(self):
        # Mock the existing MCP transport and inspect its exact operation and output permissions.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            original = root / "sealed-template.pbapp"
            original.write_bytes(b"unsigned template")
            destination = root / "copy.pbapp"
            environment = dict(PLAYBIT_PROFILE=str(root / "profile"))
            def sign(arguments, **options):
                self.assertEqual(arguments, ["offline-sandbox", str(root / "App.app/Contents/MacOS/playbit-mcp")])
                self.assertEqual(options["env"], environment)
                request = json.loads(options["input"])
                self.assertEqual(request["params"]["arguments"], dict(op="sign-package", bundle=str(destination)))
                self.assertEqual(destination.read_bytes(), b"unsigned template")
                self.assertEqual(stat.S_IMODE(destination.stat().st_mode), 0o600)
                destination.write_bytes(b"signed private copy")
                reply = dict(build_id=encode(bytes([1]) * 32))
                return SimpleNamespace(returncode=0, stdout=json.dumps(dict(id=1,
                    result=dict(content=[dict(type="text", text=json.dumps(reply))]))), stderr="")
            with patch.object(offline_fixture.subprocess, "run", side_effect=sign) as run:
                result = offline_fixture.sign_template(root / "App.app", original, destination,
                    environment, ["offline-sandbox"])
                self.assertEqual(result, destination)
                run.assert_called_once()
            self.assertEqual(original.read_bytes(), b"unsigned template")
            self.assertEqual(destination.read_bytes(), b"signed private copy")

    def test_original_output_and_failed_signing_are_never_replayed(self):
        # Existing destinations fail before dispatch. An error retains the original copied input.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            original = root / "template.pbapp"; original.write_bytes(b"template")
            copied = root / "copy.pbapp"; copied.write_bytes(b"prior")
            environment = dict(PLAYBIT_PROFILE=str(root / "profile"))
            with patch.object(offline_fixture.subprocess, "run") as run:
                with self.assertRaises(FileExistsError):
                    offline_fixture.sign_template(root, original, copied, environment, [])
                run.assert_not_called()
            with patch.object(offline_fixture.subprocess, "run", return_value=SimpleNamespace(
                    returncode=0, stdout=json.dumps(dict(id=1,result=dict(isError=True,
                        content=[dict(text="fixture signer unavailable")]))),stderr="")) as run:
                with self.assertRaisesRegex(RuntimeError, "signer unavailable"):
                    offline_fixture.sign_template(root, original, root / "failed.pbapp", environment, [])
                run.assert_called_once()
            self.assertEqual((root / "failed.pbapp").read_bytes(), b"template")
            self.assertEqual(copied.read_bytes(), b"prior")


if __name__ == "__main__":
    unittest.main()
