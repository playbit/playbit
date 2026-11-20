#!/usr/bin/env python3
"""Real access-group tests for a provisioned macOS package and synthetic account.

The default run queries exact synthetic accounts in the shipping group. The
optional AP recovery test discovers accounts only in an exclusive qualification
group. Neither changes login/default Keychains nor exports private keys.
"""
import argparse
from datetime import datetime, timezone
import fnmatch
import hashlib
import json
import os
import socket
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile

TIMEOUT = 20
OFFLINE = ["/usr/bin/sandbox-exec", "-p",
           '(version 1) (allow default) (deny network-outbound (remote ip "*:*"))']


def run(arguments, **kwargs):
    try:
        result = subprocess.run(arguments, capture_output=True, text=True, timeout=TIMEOUT, **kwargs)
    except subprocess.TimeoutExpired:
        raise RuntimeError("fixture command timed out") from None
    if result.returncode:
        # Do not include argv: security's arguments contain synthetic secrets.
        raise RuntimeError(result.stderr.strip() or "fixture command failed")
    return result.stdout


def check_engine(fixture, hold):
    ready, output = socket.socketpair()
    env = dict(os.environ, PLAYBIT_PROFILE=str(fixture.profile))
    env.pop("PLAYBIT_TESTING_CREDENTIALS", None)
    log_path = fixture.work / "engine.log"
    with log_path.open("ab") as log:
        engine = subprocess.Popen(OFFLINE + [str(fixture.engine)], env=env, stdout=output, stderr=log)
        output.close()
        try:
            ready.settimeout(TIMEOUT)
            received = bytearray()
            while len(received) < 64 and not received.endswith(b"\n"):
                part = ready.recv(64 - len(received))
                if not part:
                    break
                received.extend(part)
            if received != b'{"ready":true}\n':
                # Allow the engine to finish logging its underlying Security.framework error.
                engine.wait(timeout=TIMEOUT)
                raise RuntimeError(f"engine readiness {bytes(received)!r}; inspect {log_path}")
            client = fixture.engine.parents[4] / "MacOS/pb"
            # Real engine admission and the account worker use the production
            # SecItem backend. AP is unavailable and local readiness stays usable.
            retry = json.loads(run(OFFLINE + [str(client), "retry-account-key-backup"], env=env))
            if retry.get("account_key_backup") != "saved":
                raise RuntimeError("distribution account worker did not save the recovery copy")
            restored = json.loads(run(OFFLINE + [str(client), "saved-account"], env=env))
            # Filling the form is a local lookup; AP is never contacted.
            if not restored.get("saved_account", {}).get("account_key"):
                raise RuntimeError("expected offline saved-account form fill")
            status = json.loads(run(OFFLINE + [str(client), "status"], env=env))
            if status.get("account_key_backup") != "saved" or status.get("enrolled") is not True:
                raise RuntimeError("Keychain action changed local account readiness")
            print(f"Local offline startup, explicit backup and exact recovery lookup passed. Profile: {fixture.profile}", flush=True)
            if hold:
                input("Keep this process running for UI tests. Quit the test GUI/guests, then press Enter to clean up: ")
        finally:
            ready.close()
            if engine.poll() is None:
                engine.terminate()
            engine.wait(timeout=TIMEOUT)


def decode_profile(path):
    result = subprocess.run(["/usr/bin/security", "cms", "-D", "-i", str(path)],
                            check=True, capture_output=True, timeout=TIMEOUT)
    return plistlib.loads(result.stdout)


def entitlements(path):
    result = subprocess.run(["/usr/bin/codesign", "-d", "--entitlements", ":-", str(path)],
                            check=True, capture_output=True, timeout=TIMEOUT)
    return plistlib.loads(result.stdout)


def validate(profile, claims, identity):
    if "OSX" not in profile.get("Platform", []):
        raise ValueError("a macOS provisioning profile is required")
    expires = profile["ExpirationDate"].replace(tzinfo=timezone.utc)
    if expires <= datetime.now(timezone.utc):
        raise ValueError("provisioning profile has expired")
    certificates = {hashlib.sha1(cert).hexdigest().upper() for cert in profile["DeveloperCertificates"]}
    if identity.upper() not in certificates:
        raise ValueError("use the SHA-1 identity fingerprint of a certificate authorized by this profile")
    permitted = profile["Entitlements"]
    app_id = claims.get("com.apple.application-identifier", "")
    if not app_id or not fnmatch.fnmatchcase(app_id, permitted.get("com.apple.application-identifier", "")):
        raise ValueError("profile does not authorize the engine application ID")
    if claims.get("com.apple.developer.team-identifier") != profile["TeamIdentifier"][0]:
        raise ValueError("engine and profile team identifiers differ")
    groups = claims.get("keychain-access-groups", [])
    if len(groups) != 1 or not any(fnmatch.fnmatchcase(groups[0], item)
                                  for item in permitted.get("keychain-access-groups", [])):
        raise ValueError("expected one profile-authorized engine Keychain group")
    denied = groups[0] + ".fixture-denied"
    if not any(fnmatch.fnmatchcase(denied, item) for item in permitted.get("keychain-access-groups", [])):
        raise ValueError("profile must also authorize a different group for the negative test")
    return groups[0], denied


class ProvisionedFixture:
    def __init__(self, work, helper, engine, profile, identity):
        self.work = Path(work).resolve()
        self.profile = self.work / "profile"
        self.engine = Path(engine).resolve()
        self.source_helper = Path(helper).resolve()
        self.provision = Path(profile).resolve()
        self.identity = identity
        self.created = False
        self.neighbor_created = False

    def signed_helper(self, name, claims):
        app = self.work / (name + ".app")
        contents = app / "Contents"
        executable = contents / "MacOS/keychain-fixture"
        executable.parent.mkdir(parents=True)
        shutil.copy2(self.source_helper, executable)
        shutil.copyfile(self.provision, contents / "embedded.provisionprofile")
        # The executable must be the main executable of its own provisioned bundle.
        engine_info = plistlib.loads((self.engine.parents[1] / "Info.plist").read_bytes())
        (contents / "Info.plist").write_bytes(plistlib.dumps({
            "CFBundleIdentifier": engine_info["CFBundleIdentifier"],
            "CFBundleExecutable": "keychain-fixture", "CFBundleName": name,
            "CFBundlePackageType": "APPL", "CFBundleVersion": "1", "LSBackgroundOnly": True,
        }))
        entitlement_path = self.work / (name + ".plist")
        entitlement_path.write_bytes(plistlib.dumps(claims))
        run(["/usr/bin/codesign", "--force", "--sign", self.identity, "--timestamp=none",
             "--options", "runtime", "--entitlements", str(entitlement_path), str(app)])
        run(["/usr/bin/codesign", "--verify", "--strict", str(app)])
        return executable

    def __enter__(self):
        if self.profile.exists():
            raise ValueError("provisioned fixture requires a fresh profile")
        run(["/usr/bin/codesign", "--verify", "--strict", str(self.engine.parents[2])])
        claims = entitlements(self.engine)
        profile = decode_profile(self.provision)
        embedded = decode_profile(self.engine.parents[1] / "embedded.provisionprofile")
        if embedded["UUID"] != profile["UUID"]:
            raise ValueError("use the provisioning profile embedded in the supplied engine")
        group, denied = validate(profile, claims, self.identity)
        if run([str(self.source_helper), "group"]).strip() != group:
            raise ValueError("build the fixture helper with the engine's PLAYBIT_KEYCHAIN_ACCESS_GROUP")
        self.helper = self.signed_helper("Allowed", claims)
        self.denied = self.signed_helper("WrongGroup", dict(claims, **{"keychain-access-groups": [denied]}))
        missing = dict(claims)
        missing.pop("keychain-access-groups")
        self.missing = self.signed_helper("MissingGroup", missing)
        credential = json.loads(run([str(self.helper), "prepare", str(self.profile)]))
        try:
            self.created = True
            try:
                run([str(self.helper), "create-key", str(self.profile)], input=json.dumps(credential))
            except RuntimeError as error:
                if "status -25299" in str(error):
                    self.created = False  # A duplicate is not ours to remove.
                raise
            foreign = dict(credential, service=credential["service"] + ".foreign")
            rejected = subprocess.run([str(self.helper), "create-key", str(self.profile)],
                                      input=json.dumps(foreign), capture_output=True, text=True, timeout=TIMEOUT)
            if not rejected.returncode or "credential does not belong to this fixture" not in rejected.stderr:
                raise RuntimeError("fixture accepted a foreign service binding")
            changed = dict(credential, secret=[byte ^ 1 for byte in credential["secret"]])
            duplicate = subprocess.run([str(self.helper), "create-key", str(self.profile)],
                                       input=json.dumps(changed), capture_output=True, text=True, timeout=TIMEOUT)
            if not duplicate.returncode or "status -25299" not in duplicate.stderr:
                raise RuntimeError("fixture creation must reject overwriting an existing item")
            run([str(self.helper), "complete", str(self.profile)])
            self.neighbor = self.work / "neighbor-profile"
            adjacent = json.loads(run([str(self.helper), "prepare", str(self.neighbor)]))
            self.neighbor_created = True
            try:
                run([str(self.helper), "create-key", str(self.neighbor)], input=json.dumps(adjacent))
            except RuntimeError as error:
                if "status -25299" in str(error):
                    self.neighbor_created = False
                raise
            run([str(self.helper), "probe-key", str(self.neighbor)])
            for helper in (self.denied, self.missing):
                result = subprocess.run([str(helper), "probe-key", str(self.profile)],
                                        capture_output=True, text=True, timeout=TIMEOUT)
                if not result.returncode or "status -34018" not in result.stderr:
                    raise RuntimeError("expected errSecMissingEntitlement from the unauthorized helper")
            print("Provisioned Keychain: allowed group reads; wrong/missing groups denied", flush=True)
            return self
        except BaseException:
            self.close()
            raise

    def remove_key(self, profile):
        run([str(self.helper), "delete-key", str(profile)])
        result = subprocess.run([str(self.helper), "probe-key", str(profile)],
                                capture_output=True, text=True, timeout=TIMEOUT)
        if not result.returncode or "status -25300" not in result.stderr:
            raise RuntimeError("synthetic provisioned key was not removed")

    def close(self):
        try:
            if self.created:
                self.remove_key(self.profile)
                self.created = False
                if self.neighbor_created:
                    run([str(self.helper), "probe-key", str(self.neighbor)])
                    print("Neighboring synthetic item survived cleanup", flush=True)
        finally:
            if self.neighbor_created:
                self.remove_key(self.neighbor)
                self.neighbor_created = False

    def __exit__(self, *_):
        self.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, required=True)
    parser.add_argument("--helper", type=Path, required=True)
    parser.add_argument("--provisioning-profile", type=Path, required=True)
    parser.add_argument("--sign-identity", required=True, help="certificate SHA-1 fingerprint")
    parser.add_argument("--hold", action="store_true")
    parser.add_argument("--recovery-test", type=Path, help="release credential_recovery test executable compiled with an exclusive qualification group")
    args = parser.parse_args()
    work = Path(tempfile.mkdtemp(prefix="playbit-provisioned-", dir="/tmp")).resolve()
    print(f"Fixture artifacts: {work}", flush=True)
    engine = args.app.resolve() / "Contents/Helpers/playbit-engine.app/Contents/MacOS/playbit-engine"
    if args.recovery_test:
        claims = entitlements(engine)
        group = run([str(args.helper.resolve()), "group"]).strip()
        team = claims["com.apple.developer.team-identifier"]
        if not group.startswith(team + ".playbit.qualification."):
            raise ValueError("recovery discovery requires an exclusive qualification group")
        claims["keychain-access-groups"] = [group]
        validate(decode_profile(args.provisioning_profile), claims, args.sign_identity)
        fixture = ProvisionedFixture(work, args.helper, engine, args.provisioning_profile, args.sign_identity)
        helper = fixture.signed_helper("RecoveryHelper", claims)
        fixture.source_helper = args.recovery_test.resolve()
        test = fixture.signed_helper("RecoveryTest", claims)
        environment = dict(os.environ, PLAYBIT_PROFILE=str(work / "hooks"),
                           PLAYBIT_PROVISIONED_KEYCHAIN_HELPER=str(helper))
        subprocess.run([str(test), "--ignored", "--exact",
                        "provisioned::distribution_keychain_backup_and_explicit_recovery",
                        "--nocapture", "--test-threads=1"], env=environment, check=True, timeout=180)
        return
    with ProvisionedFixture(work, args.helper, engine, args.provisioning_profile, args.sign_identity) as fixture:
        check_engine(fixture, False)
        check_engine(fixture, args.hold)
    print("Removed only the synthetic provisioned item; absence verified", flush=True)


if __name__ == "__main__":
    main()
