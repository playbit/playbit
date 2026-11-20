import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("probe", Path(__file__).with_name("build.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "scripts"))
from identifier import decode as decode_identifier


class ProbeBuildTests(unittest.TestCase):
    def test_c_declaration_identifiers_match_the_binary_probe_ids(self):
        source = Path(__file__).with_name("main.c").read_text()
        declaration = source.split("static const uint8_t declaration[] =", 1)[1].split(";", 1)[0]
        declaration = json.loads("".join(json.loads(part) for part in
            re.findall(r'"(?:[^"\\]|\\.)*"', declaration)))
        database = declaration["databases"][0]
        self.assertEqual(decode_identifier(database["id"], 15), b"\x01" + bytes(14))
        content = database["kind"]
        type_id, = content["schema"]["types"]
        field_id, = content["schema"]["types"][type_id]["fields"]
        obj, = content["objects"]
        object_field, = obj["fields"]
        for text, value in [(type_id, 1), (field_id, 2), (obj["id"], 3),
                            (obj["type_id"], 1), (object_field, 2)]:
            self.assertEqual(decode_identifier(text), bytes([value]) + bytes(15))

    def project(self, directory, name='A "quoted" 🐚 app'):
        project = directory / "project"
        (project / "src/nested").mkdir(parents=True)
        (project / "src/lib.rs").write_text("mod nested;\n")
        (project / "src/nested/mod.rs").write_text("pub fn unrelated() {}\n")
        (project / "Cargo.toml").write_text(
            '[package]\nname = "sdk-probe-fixture"\nversion = "0.1.0"\nedition = "2024"\n')
        (project / "playbit.toml").write_text("name = " + json.dumps(name, ensure_ascii=False))
        (project / "playbit-volumes.json").write_text('{}')
        sdk = directory / "sdk"
        (sdk / "aarch64-apple-darwin").mkdir(parents=True)
        (sdk / "aarch64-apple-darwin/link.json").write_text('{"minimum_os":"15.0","link_args":[]}')
        return project, sdk

    def test_supplied_project_preserves_declaration_and_nested_source(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            project, sdk = self.project(directory)
            destination = directory / "output"
            _, library = module.stage(project, destination, sdk)
            self.assertEqual(library, Path("src/lib.rs"))
            for relative in ("Cargo.toml", "playbit.toml", "playbit-volumes.json", "src/lib.rs", "src/nested/mod.rs"):
                self.assertEqual((destination / relative).read_bytes(), (project / relative).read_bytes())
            title = (destination / "title.rs").read_text().strip()[2:-1]
            self.assertEqual(bytes(map(int, title.split(", "))).decode(), 'A "quoted" 🐚 app')
            self.assertFalse((destination / "src/app.rs").exists())

    def test_invalid_inputs_fail_before_creating_output(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            project, sdk = self.project(directory)
            destination = directory / "output"
            for name in ("", " ", "x" * 257, "🐚" * 65, "nul\0name"):
                (project / "playbit.toml").write_text("name = " + json.dumps(name, ensure_ascii=False))
                with self.assertRaises((ValueError, KeyError)):
                    module.stage(project, destination, sdk)
                self.assertFalse(destination.exists())
            (project / "playbit.toml").write_text('name = "Valid"')
            (project / "playbit-volumes.json").write_bytes(b" " * (6 * 65536 + 1))
            with self.assertRaisesRegex(ValueError, "volume declarations"):
                module.stage(project, destination, sdk)
            self.assertFalse(destination.exists())
            (project / "playbit-volumes.json").write_text('{}')
            (project / "src/lib.rs").unlink()
            with self.assertRaisesRegex(ValueError, "src/lib.rs"):
                module.stage(project, destination, sdk)
            self.assertFalse(destination.exists())

    def test_launcher_compiles_registration_without_assuming_app_state_shape(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            project, sdk = self.project(directory)
            (project / "src/lib.rs").unlink()
            with (project / "Cargo.toml").open("a") as cargo:
                cargo.write('\n[lib]\nname = "playbit_app"\npath = "src/app.rs"\n'
                            'crate-type = ["cdylib", "rlib"]\n')
            generated = project / "target/playbit"
            generated.mkdir(parents=True)
            (generated / "metadata.rs").write_text('const _: u16 = 1;\n')
            (generated / "unrelated-cache").write_bytes(b'not source')
            (project / "src/app.rs").write_text(
                'include!(concat!(env!("CARGO_MANIFEST_DIR"), "/target/playbit/metadata.rs"));\n'
                'mod nested;\nmod state;\n#[unsafe(no_mangle)]\n'
                'pub fn playbit_app() -> playbit_api::App {\n'
                '    playbit_api::app(nested::new(), nested::turn, true)\n}\n')
            (project / "src/nested/mod.rs").write_text(
                'use crate::state::State;\n'
                'pub fn new() -> State { State(0) }\n'
                'pub fn turn(value: &mut State, _: &mut playbit_api::context::Context<\'_>, _: &mut playbit_api::ui::ViewEncoder<\'_>) {\n'
                '    value.0 += 1;\n}\n')
            (project / "src/state.rs").write_text('pub struct State(pub u32);\n')
            destination = directory / "output"
            _, library = module.stage(project, destination, sdk)
            self.assertEqual(library, Path("src/app.rs"))
            self.assertEqual(list((destination / "target/playbit").iterdir()),
                             [destination / "target/playbit/metadata.rs"])
            api = destination / "libplaybit_api.rlib"
            subprocess.run([
                "rustc", "--edition=2024", "-Dwarnings", "--cfg", 'feature="guest"',
                "--crate-name", "playbit_api", "--crate-type", "rlib",
                str(destination / "playbit-api/src/lib.rs"), "-o", str(api)], check=True)
            subprocess.run([
                "rustc", "--edition=2024", "-Dwarnings", "--crate-name", "playbit_app",
                "--crate-type", "rlib", str(destination / library),
                "--extern", "playbit_api=" + str(api),
                "-o", str(destination / "libplaybit_app.rlib")], check=True,
                env=dict(os.environ, CARGO_MANIFEST_DIR=str(destination)))
            subprocess.run([
                "rustc", "--edition=2024", "-Dwarnings", "--emit=metadata",
                str(destination / "main.rs"), "--extern", "playbit_api=" + str(api),
                "--extern", "playbit_app=" + str(destination / "libplaybit_app.rlib"),
                "-Ldependency=" + str(destination),
                "-o", str(destination / "launcher.rmeta")], check=True)

    def test_unsupported_library_paths_fail_before_creating_output(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            project, sdk = self.project(directory)
            destination = directory / "output"
            manifest = (project / "Cargo.toml").read_text()
            for library in ("", "lib.rs", "../other.rs", "src/../../other.rs", "/tmp/app.rs", 3):
                (project / "Cargo.toml").write_text(
                    manifest + "\n[lib]\npath = " + json.dumps(library) + "\n")
                with self.assertRaisesRegex(ValueError, "library path"):
                    module.stage(project, destination, sdk)
                self.assertFalse(destination.exists())
            (directory / "outside.rs").write_text("pub fn outside() {}\n")
            (project / "src/escaped.rs").symlink_to(directory / "outside.rs")
            (project / "Cargo.toml").write_text(manifest + '\n[lib]\npath = "src/escaped.rs"\n')
            with self.assertRaisesRegex(ValueError, "library path"):
                module.stage(project, destination, sdk)
            self.assertFalse(destination.exists())

    def test_oversized_cargo_manifest_fails_before_creating_output(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            project, sdk = self.project(directory)
            destination = directory / "output"
            (project / "Cargo.toml").write_bytes(b"x" * 65537)
            with self.assertRaisesRegex(ValueError, "Cargo manifest"):
                module.stage(project, destination, sdk)
            self.assertFalse(destination.exists())

    def test_existing_output_and_project_tree_are_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            project, sdk = self.project(directory)
            destination = directory / "output"
            destination.mkdir()
            (destination / "sentinel.rs").write_bytes(b"retained")
            with self.assertRaises(FileExistsError):
                module.stage(project, destination, sdk)
            self.assertEqual((destination / "sentinel.rs").read_bytes(), b"retained")
            with self.assertRaisesRegex(ValueError, "outside"):
                module.stage(project, project / "output", sdk)
            self.assertFalse((project / "output").exists())

    def test_output_inside_sdk_fails_before_copying_or_creating_files(self):
        with tempfile.TemporaryDirectory() as directory:
            project, sdk = self.project(Path(directory))
            metadata = sdk / "aarch64-apple-darwin/link.json"
            original = metadata.read_bytes()
            for destination in (sdk, sdk / "output"):
                with self.assertRaisesRegex(ValueError, "outside.*SDK"):
                    module.stage(project, destination, sdk)
                self.assertEqual(metadata.read_bytes(), original)
                self.assertFalse((sdk / "output").exists())
                self.assertFalse((sdk / "src").exists())


if __name__ == "__main__":
    unittest.main()
