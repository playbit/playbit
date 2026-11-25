#!/usr/bin/env python3
"""Package the engine as the only process authorized for Playbit Keychain access."""
import fnmatch
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys

APP_ID = "playbit.engine.helper"
ENGINE = "Helpers/playbit-engine.app/Contents"


def signing_entitlements(profile):
    prefix = profile["ApplicationIdentifierPrefix"][0] + "."
    group = prefix + "playbit"
    if len(group) > 255 or not all(c.isascii() and (c.isalnum() or c in ".-") for c in group):
        raise ValueError("invalid Keychain access group")
    permitted = profile["Entitlements"]
    app_id = prefix + APP_ID
    if not fnmatch.fnmatchcase(app_id, permitted.get("com.apple.application-identifier", "")):
        raise ValueError("provisioning profile does not authorize Playbit's application ID")
    if not any(fnmatch.fnmatchcase(group, item) for item in permitted.get("keychain-access-groups", [])):
        raise ValueError("provisioning profile does not authorize the shared playbit Keychain group")
    entitlements = plistlib.loads(Path(__file__).with_name("runtime-entitlements.plist").read_bytes())
    entitlements.update({
        "com.apple.application-identifier": app_id,
        "com.apple.developer.team-identifier": profile["TeamIdentifier"][0],
        "keychain-access-groups": [group],
    })
    return entitlements


def prepare(app, profile_path, profile, entitlements_path, managed):
    # Only the engine calls SecItem. CLI, MCP and GUI clients use its existing IPC.
    # The outer app's profile cannot authorize a separate flat engine executable.
    # The engine must be its bundle's CFBundleExecutable; a compiled ID is not enough.
    # See docs/internal/macos-credentials.md for the signing constraint.
    entitlements = signing_entitlements(profile) if profile is not None else None
    contents = app / "Contents"
    info = plistlib.loads((contents / "Info.plist").read_bytes())
    if entitlements is not None and info["CFBundleIdentifier"] != "playbit.engine":
        raise ValueError("expected the canonical Playbit bundle")
    helper = contents / ENGINE
    binary = helper / "MacOS/playbit-engine"
    binary.parent.mkdir(parents=True)
    shutil.move(contents / "MacOS/playbit-engine", binary)
    engine_info = plistlib.loads(Path(__file__).with_name("engine-Info.plist").read_bytes())
    engine_info["CFBundleIdentifier"] = info["CFBundleIdentifier"] + ".helper"
    engine_info["CFBundleVersion"] = info["CFBundleVersion"]
    (helper / "Info.plist").write_bytes(plistlib.dumps(engine_info))
    if managed:
        agents = contents / "Library/LaunchAgents"
        agents.mkdir(parents=True, exist_ok=True)
        settings = {
            "BundleProgram": "Contents/" + ENGINE + "/MacOS/playbit-engine",
            # Activation is background-only, but the engine also serves local input.
            # Do not place its storage and IPC on launchd's throttled background lane.
            "ProcessType": "Interactive",
            "LimitLoadToSessionType": "Aqua",
            # Keep explicit on-demand restarts below the readiness deadline.
            "ThrottleInterval": 1,
            "ExitTimeOut": 30,
        }
        for filename, label, arguments in (
            ("playbit-engine.plist", ".agent", ["playbit-engine", "--launch-agent"]),
            ("playbit-engine-testing.plist", ".testing.agent", ["playbit-engine", "--launch-agent", "testing"]),
        ):
            (agents / filename).write_bytes(plistlib.dumps({
                **settings, "Label": engine_info["CFBundleIdentifier"] + label,
                "ProgramArguments": arguments,
            }))
    if entitlements is not None:
        shutil.copyfile(profile_path, helper / "embedded.provisionprofile")
        entitlements_path.write_bytes(plistlib.dumps(entitlements))


def main():
    configured = os.environ.get("PB_ENGINE_PROVISIONING_PROFILE")
    if bool(configured) != bool(os.environ.get("PB_PROVISIONING_PROFILE")):
        raise ValueError("Provisioned builds require both PB_PROVISIONING_PROFILE and PB_ENGINE_PROVISIONING_PROFILE")
    profile_path = Path(configured).resolve(strict=True) if configured else None
    profile = None
    if profile_path is not None:
        keychain = Path.home() / "Library/Keychains/login.keychain-db"
        decoded = subprocess.run(["security", "cms", "-D", "-k", str(keychain), "-i", str(profile_path)],
                                 check=True, capture_output=True)
        profile = plistlib.loads(decoded.stdout)
    if sys.argv[1:] == ["group"]:
        # Run before Cargo: the signed group and compiled group have one source.
        print(signing_entitlements(profile)["keychain-access-groups"][0] if profile is not None else "")
    else:
        app, output = map(Path, sys.argv[1:])
        prepare(app, profile_path, profile, output, (os.environ.get("PB_SIGN_IDENTITY") or "-") != "-")


if __name__ == "__main__":
    main()
