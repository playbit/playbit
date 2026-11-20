#!/usr/bin/env python3
"""Check outer-app provisioning and capability placement without signing credentials."""
import copy
import importlib.util
import os
from pathlib import Path
import plistlib
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("provider", Path(__file__).with_name("build-provider.py"))
provider = importlib.util.module_from_spec(spec)
spec.loader.exec_module(provider)


class ProviderSigningTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.app = self.root / "Playbit.app"
        self.contents = self.app / "Contents"
        (self.contents / "MacOS").mkdir(parents=True)
        self.output = self.root / "signing"
        provider.write(self.contents / "Info.plist", {
            "CFBundleIdentifier": "playbit.engine", "CFBundleVersion": "1",
            "CFBundleShortVersionString": "0.1.0", "UTExportedTypeDeclarations": [],
        })
        provider.write(self.contents / "Resources/runtime-entitlements.plist", {
            "com.apple.security.cs.allow-jit": True,
        })
        (self.root / "playbit-file-provider").write_bytes(b"synthetic executable")
        self.profile = self.root / "app.provisionprofile"
        self.extension_profile = self.root / "extension.provisionprofile"
        self.profile.write_bytes(b"synthetic app profile")
        self.extension_profile.write_bytes(b"synthetic extension profile")
        self.main = {
            "ApplicationIdentifierPrefix": ["PREFIX"], "TeamIdentifier": ["TEAM"],
            "Entitlements": {
                "com.apple.application-identifier": "PREFIX.playbit.engine",
                "com.apple.security.application-groups": ["group.app.playbit"],
                "com.apple.developer.aps-environment": "production",
                "com.apple.developer.associated-domains": ["*"],
            },
        }
        self.extension = copy.deepcopy(self.main)
        self.extension["Entitlements"]["com.apple.application-identifier"] += ".fileprovider"
        self.environment = {
            "PB_PROVISIONING_PROFILE": str(self.profile),
            "PB_FILE_PROVIDER_PROVISIONING_PROFILE": str(self.extension_profile),
            "PB_SIGN_IDENTITY": "synthetic identity",
        }

    def build(self, provisioned=True):
        with mock.patch.dict(os.environ, self.environment if provisioned else {}, clear=True), \
                mock.patch.object(provider, "profile", side_effect=[self.main, self.extension]), \
                mock.patch.object(provider, "compile_swift"), mock.patch.object(provider, "sign"):
            provider.build(self.app, self.output)

    def test_profile_authorizes_only_the_requested_host_domains(self):
        # Apple profiles can authorize all domains; the signature requests only our two.
        for permitted in ["*", ["*"], ["applinks:playbit.app", "applinks:testing.playbit.app"]]:
            with self.subTest(permitted=permitted):
                self.main["Entitlements"]["com.apple.developer.associated-domains"] = permitted
                self.build()
                host = plistlib.loads((self.output / "provider-host.plist").read_bytes())
                self.assertEqual(host["com.apple.application-identifier"], "PREFIX.playbit.engine")
                self.assertEqual(host["com.apple.developer.team-identifier"], "TEAM")
                self.assertEqual(host["com.apple.developer.associated-domains"],
                                 ["applinks:playbit.app", "applinks:testing.playbit.app"])
                self.assertEqual((self.contents / "embedded.provisionprofile").read_bytes(),
                                 self.profile.read_bytes())
                for name in ["provider.plist", "provider-service.plist"]:
                    claims = plistlib.loads((self.output / name).read_bytes())
                    self.assertNotIn("com.apple.developer.associated-domains", claims)

    def test_profile_must_authorize_application_and_both_domains(self):
        # Reject invalid capability grants before producing any signed component.
        for permitted in [None, [], ["applinks:playbit.app"], ["applinks:*.playbit.app"], "applinks:playbit.app"]:
            with self.subTest(permitted=permitted):
                self.main["Entitlements"]["com.apple.developer.associated-domains"] = permitted
                with self.assertRaisesRegex(ValueError, "Associated Domains"):
                    self.build()
        self.main["Entitlements"]["com.apple.application-identifier"] = "OTHER.playbit.engine"
        with self.assertRaisesRegex(ValueError, "application identifier"):
            self.build()

    def test_local_build_has_no_provisioned_capability(self):
        self.build(provisioned=False)
        host = plistlib.loads((self.output / "provider-host.plist").read_bytes())
        self.assertNotIn("com.apple.developer.associated-domains", host)
        self.assertFalse((self.contents / "embedded.provisionprofile").exists())


if __name__ == "__main__":
    unittest.main()
