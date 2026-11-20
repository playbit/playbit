#!/usr/bin/env python3
"""Build the isolated Rust artwork bridge and sandboxed Swift thumbnail extension."""
import os
from pathlib import Path
import plistlib
import re
import shlex
import subprocess
import sys

SOURCE = Path(__file__).resolve().parent / "QuickLook"


def configure(app):
    info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
    return {
        "CFBundleIdentifier": info["CFBundleIdentifier"] + ".thumbnail",
        "CFBundleName": "Playbit Thumbnail",
        "CFBundleExecutable": "PlaybitThumbnail",
        "CFBundlePackageType": "XPC!",
        "CFBundleVersion": info["CFBundleVersion"],
        "CFBundleShortVersionString": info["CFBundleShortVersionString"],
        "LSMinimumSystemVersion": info["LSMinimumSystemVersion"],
        "NSExtension": {
            "NSExtensionPointIdentifier": "com.apple.quicklook.thumbnail",
            "NSExtensionPrincipalClass": "PlaybitThumbnailProvider",
            "NSExtensionAttributes": {
                "QLSupportedContentTypes": ["app.playbit.app"],
                "QLThumbnailMinimumDimension": 0,
            },
        },
    }


def build(app, output):
    output.mkdir(parents=True, exist_ok=True)
    info = configure(app)
    contents = app / "Contents/PlugIns/PlaybitThumbnail.appex/Contents"
    (contents / "MacOS").mkdir(parents=True, exist_ok=True)
    (contents / "Info.plist").write_bytes(plistlib.dumps(info))
    # Isolate Cargo's feature resolution; default targets bring in the engine/VM.
    target = Path("target/quicklook").resolve()
    environment = dict(os.environ, CARGO_PROFILE_RELEASE_PANIC="abort")
    command = ["cargo", "rustc", "--color", "never", "--locked", "-p", "playbit",
        "--lib", "--release", "--no-default-features", "--features", "quicklook",
        "--target-dir", str(target), "--crate-type", "staticlib", "--", "-D", "warnings", "--print", "native-static-libs"]
    result = subprocess.run(command, env=environment, capture_output=True, text=True)
    (output / "rust.log").write_text(result.stdout + result.stderr)
    if result.returncode:
        raise RuntimeError(result.stdout + result.stderr)
    libraries = re.findall(r"^note: native-static-libs: (.+)$", result.stderr, re.M)
    if len(libraries) != 1 or len(libraries[0]) > 8192:
        raise ValueError("Missing or oversized Rust native linker requirements")
    # System dynamic libraries are unordered; avoid duplicate linker warnings.
    tokens = iter(shlex.split(libraries[0]))
    arguments, seen = [], set()
    for token in tokens:
        if token in ("-lSystem", "-lc", "-lm"):
            continue
        if token == "-framework":
            name = next(tokens, None)
            if name is None:
                raise ValueError("Missing framework name")
            unit = (token, name)
        elif token.startswith("-l"):
            unit = (token,)
        else:
            raise ValueError("Unsupported native linker argument")
        if unit not in seen:
            seen.add(unit)
            arguments.extend(unit)
    subprocess.run(["xcrun", "swiftc", "-swift-version", "5", "-warnings-as-errors", "-O",
        "-target", "arm64-apple-macosx" + info["LSMinimumSystemVersion"],
        "-module-cache-path", str(target / "swift-modules"),
        "-application-extension", "-parse-as-library", "-Xlinker", "-e", "-Xlinker", "_NSExtensionMain",
        "-import-objc-header", str(SOURCE / "ThumbnailBridge.h"),
        str(SOURCE / "ThumbnailProvider.swift"), str(target / "release/libplaybit.a"),
        *arguments, "-o", str(contents / "MacOS/PlaybitThumbnail")], check=True)
    entitlements = output / "entitlements.plist"
    entitlements.write_bytes(plistlib.dumps({"com.apple.security.app-sandbox": True}))
    identity = os.environ.get("PB_SIGN_IDENTITY", "-")
    signing = ["codesign", "--force", "--sign", identity, "--entitlements", str(entitlements)]
    if identity != "-":
        signing += ["--timestamp", "--options", "runtime"]
    subprocess.run([*signing, str(contents.parent)], check=True)


if __name__ == "__main__":
    build(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())
