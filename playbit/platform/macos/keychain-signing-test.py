#!/usr/bin/env python3
"""Check provisioned helper layout without using a real signing identity."""
import importlib.util
from pathlib import Path
import plistlib
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("signing", Path(__file__).with_name("keychain-signing.py"))
signing = importlib.util.module_from_spec(spec)
spec.loader.exec_module(signing)


class SigningTests(unittest.TestCase):
    def test_only_engine_has_bundle_and_authorized_group(self):
        # Both build types use identical paths; only the provisioned engine gets a profile.
        for provisioned, managed in ((False, False), (False, True), (True, True)):
            with self.subTest(provisioned=provisioned, managed=managed), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                app = root / "Playbit.app"
                contents = app / "Contents"
                (contents / "MacOS").mkdir(parents=True)
                for name in ("Playbit", "playbit-engine", "pb", "playbit-mcp"):
                    (contents / "MacOS" / name).write_text(name)
                (contents / "Info.plist").write_bytes(plistlib.dumps({"CFBundleIdentifier": "playbit.engine", "CFBundleVersion": "42"}))
                profile_path = root / "profile"
                profile_path.write_bytes(b"synthetic provisioning profile")
                profile = {
                    "ApplicationIdentifierPrefix": ["TEAM"], "TeamIdentifier": ["TEAM"],
                    "Entitlements": {"com.apple.application-identifier": "TEAM." + signing.APP_ID,
                                     "keychain-access-groups": ["TEAM.*"]},
                }
                entitlements = root / "entitlements.plist"
                signing.prepare(app, profile_path, profile if provisioned else None, entitlements, managed)
                self.assertEqual(sorted(p.name for p in (contents / "MacOS").iterdir()),
                                 ["Playbit", "pb", "playbit-mcp"])
                self.assertFalse(any(p.is_symlink() for p in app.rglob("*")))
                self.assertEqual([p.name for p in (contents / "Helpers").iterdir()], ["playbit-engine.app"])
                for name in ("Playbit", "pb", "playbit-mcp"):
                    self.assertEqual((contents / "MacOS" / name).read_text(), name)
                helper = contents / signing.ENGINE
                self.assertEqual((helper / "MacOS/playbit-engine").read_text(), "playbit-engine")
                info = plistlib.loads((helper / "Info.plist").read_bytes())
                self.assertEqual(info["CFBundleIdentifier"], signing.APP_ID)
                self.assertEqual(info["CFBundleExecutable"], "playbit-engine")
                self.assertEqual(info["CFBundleVersion"], "42")
                self.assertTrue(info["LSBackgroundOnly"])
                if managed:
                    for filename, label, arguments in (
                        ("playbit-engine.plist", ".agent", ["playbit-engine", "--launch-agent"]),
                        ("playbit-engine-testing.plist", ".testing.agent", ["playbit-engine", "--launch-agent", "testing"]),
                    ):
                        agent = plistlib.loads((contents / "Library/LaunchAgents" / filename).read_bytes())
                        self.assertEqual(agent["Label"], signing.APP_ID + label)
                        self.assertEqual(agent["BundleProgram"], "Contents/" + signing.ENGINE + "/MacOS/playbit-engine")
                        self.assertEqual(agent["ProgramArguments"], arguments)
                        self.assertEqual(agent["ProcessType"], "Interactive")
                        self.assertIs(type(agent["ThrottleInterval"]), int)
                        self.assertEqual(agent["ThrottleInterval"], 1)
                        self.assertNotIn("RunAtLoad", agent)
                        self.assertNotIn("KeepAlive", agent)
                self.assertNotIn("PlaybitKeychainAccessGroup", info)
                self.assertFalse((contents / "embedded.provisionprofile").exists())
                self.assertEqual((helper / "embedded.provisionprofile").exists(), provisioned)
                self.assertEqual(entitlements.exists(), provisioned)
                if provisioned:
                    self.assertEqual(plistlib.loads(entitlements.read_bytes())["keychain-access-groups"], ["TEAM.playbit"])
                profile["Entitlements"]["keychain-access-groups"] = ["OTHER.*"]
                with self.assertRaisesRegex(ValueError, "shared playbit"):
                    signing.signing_entitlements(profile)
                profile["ApplicationIdentifierPrefix"] = ["bad/prefix"]
                with self.assertRaisesRegex(ValueError, "invalid Keychain"):
                    signing.signing_entitlements(profile)


if __name__ == "__main__":
    unittest.main()
