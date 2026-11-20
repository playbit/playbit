import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import re
import tomllib
import unittest

spec = importlib.util.spec_from_file_location("resources", Path(__file__).with_name("developer-resources.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class DeveloperResourcesTests(unittest.TestCase):
    def test_todo_starter_selects_session_with_only_declared_features(self):
        repo = Path(__file__).resolve().parents[3]
        with tempfile.TemporaryDirectory() as directory:
            original = Path(directory) / "Original.app/Contents"
            (original / "MacOS").mkdir(parents=True)
            (original / "MacOS/pb").write_bytes(b"fixture executable")
            (original / "Helpers").mkdir()
            resources = original / "Resources"
            for name in ("playbit-api", "native-sdk", "project-template/src"):
                (resources / name).mkdir(parents=True)
            incoming = resources / "project-template"
            cargo = (repo / "playbit/sdk/rust/template/Cargo.toml").read_bytes()
            (incoming / "Cargo.toml").write_bytes(cargo)
            module.write_template_guest(incoming, "little-list")
            destination = Path(directory) / "TODO.app"
            module.stage_todo_fixture(original.parent, destination)
            template = destination / "Contents/Resources/project-template"
            self.assertEqual(sorted(path.name for path in (template / "src").iterdir()), ["app.rs"])
            self.assertEqual((template / "Cargo.toml").read_bytes(), cargo)
            app = (template / "src/app.rs").read_text()
            features = tomllib.loads((repo / "playbit/sdk/rust/template/Cargo.toml").read_text())["features"]
            used = set(re.findall(r'feature\s*=\s*"([^"\n]+)"', app))
            self.assertLessEqual(used, set(features), "staged guest must compile with check-cfg warnings denied")
            self.assertEqual(app.count("let root = available.volume(VolumeRole::Session);"), 1)
            self.assertIn("let root = root.ok_or(Error::NotFound)?.handle;", app)
            self.assertEqual((template / "playbit-volumes.json").read_bytes(),
                             (repo / "guests/todo/playbit-volumes.json").read_bytes())

    def test_installed_crate_and_template_match_sources(self):
        # Stage the actual resources and compare the SDK bytes with this build's source.
        repo = Path(__file__).resolve().parents[3]
        with tempfile.TemporaryDirectory() as directory:
            resources = Path(directory)
            version = tomllib.loads((repo / "Cargo.toml").read_text())["workspace"]["package"]["version"]
            module.stage(resources, version)
            api = resources / "playbit-api"
            for path in api.rglob("*"):
                if path.is_file() and path.name != "Cargo.toml":
                    relative = path.relative_to(api)
                    source = (repo / "playbit" / relative if relative.parts[0] == "docdb"
                              else repo / "playbit-api" / relative)
                    self.assertEqual(path.read_bytes(), source.read_bytes())
            manifest = tomllib.loads((api / "Cargo.toml").read_text())
            self.assertEqual(manifest["package"]["version"], version)
            self.assertNotIn("workspace", manifest["package"])
            docdb = api / manifest["dependencies"]["docdb"]["path"]
            docdb_manifest = tomllib.loads((docdb / "Cargo.toml").read_text())
            self.assertEqual(docdb_manifest["package"]["version"], version)
            self.assertTrue((docdb / docdb_manifest["lib"]["path"]).is_file())
            template = resources / "project-template"
            target = next(line for line in (repo / ".cargo/config.toml").read_text().splitlines()
                          if line.startswith("MACOSX_DEPLOYMENT_TARGET = "))
            self.assertIn("[env]\n" + target + "\n", (template / ".cargo/config.toml").read_text())
            guest_src = repo / "guests/little-list/src"
            sources = {path.relative_to(guest_src): path.read_bytes()
                       for path in guest_src.rglob("*") if path.is_file()}
            entry = sources.pop(Path("lib.rs")).decode().removeprefix(
                '#[cfg(not(target_arch = "wasm32"))]\nextern crate playbit;\n\n')
            sources[Path("app.rs")] = entry.encode()
            self.assertEqual(
                {path.relative_to(template / "src"): path.read_bytes()
                 for path in (template / "src").rglob("*") if path.is_file()}, sources)
            app = (template / "src/app.rs").read_text()
            self.assertIn("pub fn playbit_app()", app)
            files = [path for path in template.rglob("*") if path.is_file()]
            self.assertEqual(sorted(str(path.relative_to(template)) for path in files), sorted([
                ".cargo/config.toml", ".gitignore", "Cargo.toml", "icon.svg",
                "playbit-volumes.json", "playbit.toml"] + [f"src/{path}" for path in sources]))
            self.assertEqual((template / "playbit-volumes.json").read_bytes(),
                             (repo / "guests/little-list/playbit-volumes.json").read_bytes())
            self.assertNotIn("root", tomllib.loads((template / "playbit.toml").read_text()))
            self.assertLess(sum(path.stat().st_size for path in files), 512 * 1024)
            self.assertEqual((template / "playbit.toml").read_bytes(), (repo / "guests/little-list/playbit.toml").read_bytes())
            self.assertTrue((resources / "native-sdk/rust/main.rs").is_file())
            self.assertFalse((resources / "native-sdk/rust/build.rs").exists())
            self.assertFalse((template / "build.rs").exists())
            manifest = tomllib.loads((template / "Cargo.toml").read_text())
            self.assertEqual(list(manifest["dependencies"]), ["playbit-api"])
            self.assertNotIn("build-dependencies", manifest)
            ocaml = resources / "ocaml-template"
            for name in ("src/app.ml", "playbit.toml", "playbit-volumes.json"):
                self.assertEqual((ocaml / name).read_bytes(),
                                 (repo / "guests/counter-ocaml" / name).read_bytes())
            self.assertEqual(sorted(str(path.relative_to(ocaml)) for path in ocaml.rglob("*") if path.is_file()),
                             [".gitignore", "icon.svg", "playbit-volumes.json", "playbit.toml", "src/app.ml"])

            # Compile the default API and starter without any registry cache or network access.
            probe = resources / "api-probe"
            probe.mkdir()
            (probe / "Cargo.toml").write_text(
                '[package]\nname="installed-api-probe"\nversion="0.0.0"\nedition="2024"\n'
                '[lib]\npath="probe.rs"\ncrate-type=["cdylib"]\n[dependencies]\n'
                f'playbit-api={{path={json.dumps(str(api))},features=["guest"]}}\n[workspace]\n')
            (probe / "probe.rs").write_text(
                '#[unsafe(no_mangle)]\npub fn playbit_app() -> playbit_api::App {\n'
                '    playbit_api::app(playbit_api::docdb::Doc::new(), |_, _, _| {}, true)\n}\n')
            shutil.copytree(api, template / "target/playbit/sdk/playbit-api")
            shutil.copyfile(resources / "native-sdk/rust/main.rs", template / "target/playbit/sdk/native-main.rs")
            cargo = template / "Cargo.toml"
            cargo.write_text(cargo.read_text()
                             .replace("@PLAYBIT_API@", "target/playbit/sdk/playbit-api")
                             .replace("@PLAYBIT_NATIVE_MAIN@", "target/playbit/sdk/native-main.rs"))
            launcher = shutil.which("rustc")
            self.assertIsNotNone(launcher, "the staged SDK test requires Rust")
            launcher = os.path.abspath(launcher)
            sysroot = Path(subprocess.check_output([launcher, "--print", "sysroot"], text=True).strip())
            # Rustup supplies macOS loader paths for compiler children such as rust-lld.
            # Preserve the launcher while pinning its already installed toolchain.
            environment = dict(os.environ, CARGO_HOME=str(resources / "empty-cargo-home"),
                               RUSTC=launcher, RUSTDOC=str(sysroot / "bin/rustdoc"),
                               RUSTUP_TOOLCHAIN=str(sysroot),
                               CARGO_NET_OFFLINE="true", RUSTUP_AUTO_INSTALL="0",
                               CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS="true",
                               CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS="true")
            for project in (probe, template):
                with self.subTest(project=project.name):
                    result = subprocess.run(
                        [str(sysroot / "bin/cargo"), "rustc", "--offline", "--release", "--lib",
                         "--crate-type", "cdylib", "--target", "wasm32-unknown-unknown"],
                        cwd=project, env=dict(environment, CARGO_TARGET_DIR=str(project / "output")),
                        capture_output=True, text=True, timeout=120)
                    self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
