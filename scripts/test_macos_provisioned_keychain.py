"""Provisioning boundary tests without credentials or signing operations."""
from datetime import datetime, timedelta, timezone
import hashlib
import tempfile
import unittest
from unittest.mock import patch

import macos_provisioned_keychain as fixture


class ProvisionedTests(unittest.TestCase):
    def setUp(self):
        self.identity = hashlib.sha1(b"fixture certificate").hexdigest().upper()
        self.profile = {
            "Platform": ["OSX"], "ExpirationDate": datetime.now(timezone.utc) + timedelta(days=1),
            "DeveloperCertificates": [b"fixture certificate"], "TeamIdentifier": ["TEAM"],
            "Entitlements": {"com.apple.application-identifier": "TEAM.app.*",
                             "keychain-access-groups": ["TEAM.*"]},
        }
        self.claims = {"com.apple.application-identifier": "TEAM.app.fixture",
                       "com.apple.developer.team-identifier": "TEAM",
                       "keychain-access-groups": ["TEAM.playbit"]}

    def test_authorized_group_and_negative_group(self):
        self.assertEqual(fixture.validate(self.profile, self.claims, self.identity),
                         ("TEAM.playbit", "TEAM.playbit.fixture-denied"))

    def test_rejects_foreign_identity(self):
        with self.assertRaisesRegex(ValueError, "certificate authorized"):
            fixture.validate(self.profile, self.claims, "0" * 40)

    def test_rejects_foreign_group_and_team(self):
        for key, value in [("keychain-access-groups", ["OTHER.playbit"]),
                           ("com.apple.developer.team-identifier", "OTHER"),
                           ("com.apple.application-identifier", "OTHER.app.fixture")]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                fixture.validate(self.profile, dict(self.claims, **{key: value}), self.identity)

    def test_rejects_missing_or_multiple_groups(self):
        for groups in ([], ["TEAM.playbit", "TEAM.other"]):
            with self.subTest(groups=groups), self.assertRaisesRegex(ValueError, "one profile-authorized"):
                fixture.validate(self.profile, dict(self.claims, **{"keychain-access-groups": groups}), self.identity)

    def test_rejects_expired_or_non_macos_profile(self):
        for key, value in [("ExpirationDate", datetime.now(timezone.utc) - timedelta(days=1)),
                           ("Platform", ["iOS"])]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                fixture.validate(dict(self.profile, **{key: value}), self.claims, self.identity)

    def test_existing_profile_rejected_before_signing(self):
        with tempfile.TemporaryDirectory() as work:
            item = fixture.ProvisionedFixture(work, "/helper", "/engine", "/profile", self.identity)
            item.profile.mkdir()
            with patch.object(fixture, "run") as run:
                with self.assertRaisesRegex(ValueError, "fresh profile"):
                    item.__enter__()
                run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
