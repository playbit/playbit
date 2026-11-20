#!/usr/bin/env python3
"""Stage version-matched language bindings and starters alongside packaged pb."""
from pathlib import Path
import fcntl
import shutil
import sys
import subprocess
import os


def stage(resources, version, guest_name="little-list"):
    repo = Path(__file__).resolve().parents[3]
    resources.mkdir(parents=True, exist_ok=True)
    api = resources / "playbit-api"
    # Ship the crate's actual source tree, not an independently maintained file list.
    api.mkdir()
    for name in ("Cargo.toml", "LICENSE", "README.md"):
        shutil.copyfile(repo / "playbit-api" / name, api / name)
    # Resolve the workspace version for the installed standalone SDK crate.
    manifest = api / "Cargo.toml"
    text = manifest.read_text()
    if text.count("version.workspace = true") != 1:
        raise ValueError("SDK version must inherit the root workspace version")
    text = text.replace('path = "../playbit/docdb"', 'path = "docdb"')
    manifest.write_text(text.replace("version.workspace = true", f'version = "{version}"'))
    docdb = api / "docdb"
    docdb.mkdir()
    shutil.copyfile(repo / "playbit/docdb/README.md", docdb / "README.md")
    docdb_manifest = (repo / "playbit/docdb/Cargo.toml").read_text()
    # Relative path crates join the app's workspace, including their dev-dependencies.
    # Install the runtime manifest without repository-only tests and generator targets.
    runtime_manifest = []
    include = True
    for line in docdb_manifest.splitlines(keepends=True):
        if line.startswith("["):
            include = line.strip() not in ("[[example]]", "[dev-dependencies]")
        if include:
            runtime_manifest.append(line)
    (docdb / "Cargo.toml").write_text(
        "".join(runtime_manifest).replace("version.workspace = true", f'version = "{version}"'))
    shutil.copytree(repo / "playbit/docdb/src", docdb / "src")
    for name in ("src", "include"):
        shutil.copytree(repo / "playbit-api" / name, api / name)
    skill = resources / "skills/playbit"
    shutil.copytree(repo / "playbit/skills/playbit", skill)
    native = resources / "native-sdk/rust"
    shutil.copytree(repo / "playbit/platform/native/cargo", native)
    template = resources / "project-template"
    shutil.copytree(repo / "playbit/sdk/rust/template", template)
    # Copy the build's forced deployment target into the standalone project.
    target = [line for line in (repo / ".cargo/config.toml").read_text().splitlines()
              if line.startswith("MACOSX_DEPLOYMENT_TARGET = ")]
    if len(target) != 1:
        raise ValueError("expected one macOS deployment target in .cargo/config.toml")
    config = template / ".cargo/config.toml"
    config.write_text("[env]\n" + target[0] + "\n\n" + config.read_text())
    (template / "src").mkdir()
    write_template_guest(template, guest_name)
    shutil.copyfile(repo / "playbit/assets/check-on.svg", template / "icon.svg")
    ocaml_api = resources / "ocaml-sdk"
    generator_dir = repo / "target/ocaml-bindgen"
    generator_dir.mkdir(parents=True, exist_ok=True)
    environment = dict(os.environ, CARGO_TARGET_DIR=str(generator_dir),
                       SCCACHE_SERVER_UDS=os.environ.get("SCCACHE_SERVER_UDS", str(repo / "_sccache.sock")), CARGO_BUILD_JOBS="2")
    # Concurrent fixtures must not replace the executable between build and use.
    with (generator_dir / "resources.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        generator = generator_dir / "debug/playbit-ocaml-bindgen"
        subprocess.run(["cargo", "build", "--quiet", "--locked", "--manifest-path", str(repo / "playbit/sdk/ocaml/bindgen/Cargo.toml")], check=True, env=environment)
        subprocess.run([str(generator), str(repo / "playbit-api"), str(ocaml_api)], check=True, env=environment)
    for name in ("playbit.ml", "playbit.mli"):
        shutil.copyfile(repo / "playbit/sdk/ocaml" / name, ocaml_api / name)
    starter = resources / "ocaml-template"
    shutil.copytree(repo / "playbit/sdk/ocaml/template", starter)
    (starter / "src").mkdir()
    guest = repo / "guests/counter-ocaml"
    for name in ("src/app.ml", "playbit.toml", "playbit-volumes.json"):
        shutil.copyfile(guest / name, starter / name)
    shutil.copyfile(repo / "playbit/assets/check-on.svg", starter / "icon.svg")


def write_template_guest(template, guest_name):
    repo = Path(__file__).resolve().parents[3]
    guest = repo / "guests" / guest_name
    if guest_name == "little-list":
        shutil.copytree(guest / "src", template / "src", dirs_exist_ok=True)
        (template / "src/lib.rs").unlink()
        # The installed SDK supplies native linking without the implementation crate.
        app = (guest / "src/lib.rs").read_text().removeprefix(
            '#[cfg(not(target_arch = "wasm32"))]\nextern crate playbit;\n\n')
    elif guest_name == "todo":
        # Existing tests retain TODO as their fixture.
        app = (guest / "src/app.rs").read_text()
        # The standalone starter keeps the strict Session selector, without the
        # repository-only ordinary-Root regression branch or Cargo feature.
        fixture_selector = '        #[cfg(feature = "fixture-root")]\n'
        session_selector = '        #[cfg(not(feature = "fixture-root"))]\n'
        if app.count(fixture_selector) != 1 or app.count(session_selector) != 1:
            raise ValueError("expected one explicit TODO fixture/default scope selector")
        before, _, selectors = app.partition(fixture_selector)
        _, _, after = selectors.partition(session_selector)
        if not after:
            raise ValueError("TODO default scope selector must follow its fixture selector")
        app = before + after
        app = app.replace("pub struct App {", "struct App {")
        app += ('\n#[allow(unsafe_code)]\n#[unsafe(no_mangle)]\n'
                'pub fn playbit_app() -> playbit_api::App {\n'
                '    playbit_api::app(App::new(), App::turn, true)\n}\n')
    else:
        raise ValueError(f"unsupported template guest: {guest_name}")
    (template / "src/app.rs").write_text(app)
    for name in ("playbit.toml", "playbit-volumes.json"):
        shutil.copyfile(guest / name, template / name)


def stage_todo_fixture(app, destination):
    # Give init its own TODO template while retaining the packaged SDK and engine.
    contents = destination / "Contents"
    (contents / "MacOS").mkdir(parents=True)
    shutil.copy2(app / "Contents/MacOS/pb", contents / "MacOS/pb")
    (contents / "Helpers").symlink_to(app / "Contents/Helpers", target_is_directory=True)
    resources = contents / "Resources"
    resources.mkdir()
    for name in ("playbit-api", "native-sdk"):
        (resources / name).symlink_to(app / "Contents/Resources" / name, target_is_directory=True)
    template = resources / "project-template"
    shutil.copytree(app / "Contents/Resources/project-template", template)
    # Replace the default guest's source tree, including generated modules.
    shutil.rmtree(template / "src")
    (template / "src").mkdir()
    write_template_guest(template, "todo")


if __name__ == "__main__":
    if sys.argv[1] == "--todo-fixture":
        stage_todo_fixture(Path(sys.argv[2]), Path(sys.argv[3]))
    else:
        stage(Path(sys.argv[1]), sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else "little-list")
