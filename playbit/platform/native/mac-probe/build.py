#!/usr/bin/env python3
"""Build external C and Rust SDK conformance probes.

Run after build-sdk.sh: python3.11 playbit/platform/native/mac-probe/build.py APP_PROJECT DEST
APP_PROJECT supplies Cargo.toml, playbit.toml and an app library in src.
DEST must be a new directory outside the checkout, APP_PROJECT and SDK. Stable and
nightly toolchains must already be installed. PLAYBIT_NATIVE_SDK selects an
installed SDK. Checkout reads and outbound IP are denied while linking.
"""
import argparse
from pathlib import Path
import json
import os
import plistlib
import shutil
import subprocess
import tomllib


def stage(project, base, sdk):
    source = Path(__file__).resolve().parent
    repo = source.parents[3]
    project, base, sdk = project.resolve(), base.resolve(), sdk.resolve()
    if any(base == root or root in base.parents for root in (repo, project, sdk)):
        raise ValueError("probe output must be outside the checkout, app project and SDK")
    with (project / "playbit.toml").open("rb") as stream:
        manifest = stream.read(320 * 1024 + 1)
    if len(manifest) > 320 * 1024:
        raise ValueError("source manifest exceeds 320 KiB")
    name = tomllib.loads(manifest.decode()).get("name")
    if not isinstance(name, str) or not name.strip() or "\0" in name or len(name.encode()) > 256:
        raise ValueError("app name must contain 1 to 256 UTF-8 bytes without NUL")
    with (project / "playbit-volumes.json").open("rb") as stream:
        initializers = stream.read(6 * 65536 + 1)
    if len(initializers) > 6 * 65536:
        raise ValueError("volume declarations exceed 393216 bytes")
    roles = json.loads(initializers)
    if not isinstance(roles, dict) or not set(roles).issubset({"root", "session", "user", "user-session", "local", "local-session"}):
        raise ValueError("volume declarations must be a role map")
    # The released native embedding entry point still accepts one ordinary Root.
    declaration = json.dumps(roles.get("root", {"permissions": {"grants": []}, "databases": [], "blobs": []}), separators=(",", ":")).encode()
    if len(declaration) > 65536:
        raise ValueError("native root declaration exceeds 65536 bytes")
    with (project / "Cargo.toml").open("rb") as stream:
        cargo = stream.read(65537)
    if len(cargo) > 65536:
        raise ValueError("Cargo manifest exceeds 64 KiB")
    library = tomllib.loads(cargo.decode()).get("lib", {})
    if not isinstance(library, dict):
        raise ValueError("Cargo lib must be a table")
    library = library.get("path", "src/lib.rs")
    if not isinstance(library, str) or not library or "\0" in library:
        raise ValueError("Cargo library path must name a source file inside src")
    library = Path(library)
    if (library.is_absolute() or not library.parts or library.parts[0] != "src"
            or ".." in library.parts
            or not (project / library).resolve().is_relative_to((project / "src").resolve())):
        raise ValueError("Cargo library path must name a source file inside src")
    if not (project / library).is_file():
        raise ValueError(f"app library is missing: {library}")
    generated = project / "target/playbit/metadata.rs"
    generated_bytes = None
    if generated.is_file():
        with generated.open("rb") as stream:
            generated_bytes = stream.read(32769)
        if len(generated_bytes) > 32768:
            raise ValueError("generated metadata exceeds 32 KiB")
    metadata = json.loads((sdk / "aarch64-apple-darwin/link.json").read_bytes())

    base.mkdir(parents=True, exist_ok=False)
    shutil.copytree(sdk, base / "sdk")
    shutil.copytree(repo / "playbit-api/src", base / "playbit-api/src")
    for file in ("main.c", "main.rs", "scenes.rs"):
        shutil.copyfile(source / file, base / file)
    shutil.copytree(project / "src", base / "src")
    if generated_bytes is not None:
        (base / "target/playbit").mkdir(parents=True)
        (base / "target/playbit/metadata.rs").write_bytes(generated_bytes)
    (base / "playbit-volumes.json").write_bytes(initializers)
    (base / "native-root.json").write_bytes(declaration)
    (base / "playbit.toml").write_bytes(manifest)
    (base / "Cargo.toml").write_bytes(cargo)
    title = ", ".join(map(str, name.encode()))
    (base / "title.rs").write_text(f"&[{title}]\n")
    return metadata, library


def build(project, base, sdk):
    metadata, library = stage(project, base, sdk)
    repo = Path(__file__).resolve().parents[4]
    policy = ('(version 1) (allow default) (deny network-outbound (remote ip "*:*")) '
              '(deny file-read* (subpath ' + json.dumps(str(repo)) + '))')
    wrap = ["/usr/bin/sandbox-exec", "-p", policy]
    env = dict(os.environ, MACOSX_DEPLOYMENT_TARGET=metadata["minimum_os"], CARGO_MANIFEST_DIR=str(base))

    def run(arguments, **kwargs):
        return subprocess.run(wrap + arguments, cwd=base, env=env, check=True, **kwargs)

    def bundle(name):
        app = base / f"{name}.app"
        executable = app / "Contents/MacOS" / name
        executable.parent.mkdir(parents=True)
        shutil.copytree(base / "sdk/licenses", app / "Contents/Resources/licenses")
        with (app / "Contents/Info.plist").open("wb") as output:
            plistlib.dump(dict(CFBundleIdentifier="playbit.sdk-probe." + name,
                              CFBundleName=name, CFBundleExecutable=name,
                              CFBundlePackageType="APPL", CFBundleVersion="1",
                              LSMinimumSystemVersion=metadata["minimum_os"],
                              NSHighResolutionCapable=True), output)
        return app, executable

    def sign(app):
        run(["codesign", "--force", "--sign", "-", str(app)])
        run(["codesign", "--verify", "--deep", "--strict", str(app)])

    for channel in ("stable", "nightly"):
        rustc = ["rustup", "run", channel, "rustc", "--edition=2024", "-Dwarnings", "-Copt-level=3"]
        run(["rustup", "run", channel, "rustc", "--version"])
        api = base / f"libplaybit_api_{channel}.rlib"
        run(rustc + ["--cfg", 'feature="guest"', "--crate-name", "playbit_api", "--crate-type", "rlib",
                     str(base / "playbit-api/src/lib.rs"), "-o", str(api)])
        application = base / f"libplaybit_app_{channel}.rlib"
        run(rustc + ["--crate-name", "playbit_app", "--crate-type", "rlib",
                     str(base / library), "--extern", "playbit_api=" + str(api),
                     "-o", str(application)])
        if channel == "stable":
            scenes = base / "scenes"
            run(rustc + [str(base / "scenes.rs"), "--extern", "playbit_api=" + str(api), "-o", str(scenes)])
            with (base / "scenes.h").open("w") as output:
                run([str(scenes)], stdout=output)
        app, executable = bundle("rust-" + channel)
        flags = [arg for token in metadata["link_args"] for arg in ("-C", "link-arg=" + token)]
        run(rustc + [str(base / "main.rs"), "--extern", "playbit_api=" + str(api),
                     "--extern", "playbit_app=" + str(application), "-Ldependency=" + str(base),
                     "-Lnative=" + str(base / "sdk/aarch64-apple-darwin"), "-lstatic=playbit",
                     *flags, "-C", "link-arg=-Wl,-fatal_warnings", "-o", str(executable)])
        sign(app)
    app, executable = bundle("c-app")
    run(["cc", "-std=c17", "-Wall", "-Wextra", "-Werror", "-Wl,-fatal_warnings", "-O2",
         "-mmacosx-version-min=" + metadata["minimum_os"], "-I", str(base / "sdk/include"),
         str(base / "main.c"), str(base / "sdk/aarch64-apple-darwin/libplaybit.a"),
         *metadata["link_args"], "-o", str(executable)])
    sign(app)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("project", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[4]
    sdk = Path(os.environ.get("PLAYBIT_NATIVE_SDK", str(repo / "target/native-mac/sdk")))
    base = args.destination.resolve()
    build(args.project, base, sdk)
    print(base)


if __name__ == "__main__":
    main()
