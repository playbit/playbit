"""Check forced target propagation through Cargo and native compiler output."""
from pathlib import Path
import os
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


@unittest.skipUnless(sys.platform == "darwin", "helper sets the target only on macOS")
class MacosTargetTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        (self.root / ".cargo").mkdir()
        (self.root / "scripts").mkdir()
        self.config = self.root / ".cargo/config.toml"
        shutil.copyfile(ROOT / "scripts/macos-env.sh", self.root / "scripts/macos-env.sh")

    def tearDown(self):
        self.temporary.cleanup()

    def configure(self, entry):
        self.config.write_text("[env]\nMACOSX_DEPLOYMENT_TARGET = " + entry + "\n")

    def read_target(self, root):
        return subprocess.run(
            ["sh", "-ec", '. scripts/macos-env.sh; printf "%s\\n" "$MACOSX_DEPLOYMENT_TARGET"'],
            cwd=root, env=dict(os.environ, MACOSX_DEPLOYMENT_TARGET="99.0"),
            capture_output=True, text=True)

    def test_reads_changed_config_without_using_inherited_environment(self):
        # The sourced helper follows the checkout's entry, overriding inherited values.
        self.configure('{ value = "15.1", force = true }')
        result = self.read_target(self.root)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "15.1")

    def test_rejects_unforced_or_invalid_targets(self):
        # Fail before producing a target that could disagree with Cargo or inject flags.
        for entry in ('"15.0"', '{ value = "15.0", force = false }',
                      '{ value = "15.0" }', '{ value = "15.0\\n-flag", force = true }',
                      '{ value = 15, force = true }'):
            with self.subTest(entry=entry):
                self.configure(entry)
                result = self.read_target(self.root)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")

    def test_missing_configuration_fails(self):
        # A missing target cannot fall back to the compiler's lower default.
        self.config.write_text("[env]\n")
        result = self.read_target(self.root)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")

    def test_duplicate_targets_fail(self):
        # A second matching entry must not produce a multi-line compiler argument.
        self.configure('{ value = "15.1", force = true }')
        self.config.write_text(self.config.read_text() + 'MACOSX_DEPLOYMENT_TARGET = { value = "15.2", force = true }\n')
        result = self.read_target(self.root)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")

    def test_configured_target_is_forced(self):
        # The source entry must retain the forced policy the native helper enforces.
        result = self.read_target(ROOT)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertRegex(result.stdout.strip(), r"^[0-9]+\.[0-9]+(?:\.[0-9]+)?$")

    def test_cargo_rust_c_objc_and_swift_embed_the_same_changed_target(self):
        # Inspect actual Mach-O output, using a changed config and conflicting env.
        # Cargo does not inherit the shell helper here; it must apply its own [env].
        self.configure('{ value = "15.1", force = true }')
        (self.root / "main.rs").write_text("fn main() {}\n")
        (self.root / "main.c").write_text("int main(void) { return 0; }\n")
        (self.root / "main.m").write_text("int main(void) { return 0; }\n")
        (self.root / "main.swift").write_text('print("target")\n')
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "target-probe"\nversion = "0.0.0"\nedition = "2024"\n'
            '[[bin]]\nname = "target-probe"\npath = "main.rs"\n')
        env = dict(os.environ, MACOSX_DEPLOYMENT_TARGET="99.0")
        env.pop("CARGO_BUILD_TARGET", None)
        env.pop("CARGO_ENCODED_RUSTFLAGS", None)
        subprocess.run(["cargo", "build", "--offline", "--target-dir", "target"],
                       cwd=self.root, env=env, check=True, capture_output=True)
        subprocess.run(["sh", "-ec", '''
. scripts/macos-env.sh
rustc main.rs -o rust
cc -std=c17 -Wall -Wextra -Werror main.c -o c
clang -fobjc-arc -Wall -Wextra -Werror main.m -o objc
swiftc -target "$(uname -m)-apple-macosx$MACOSX_DEPLOYMENT_TARGET" \
    -module-cache-path module-cache main.swift -o swift
'''], cwd=self.root, env=env, check=True, capture_output=True)
        for name in ("target/debug/target-probe", "rust", "c", "objc", "swift"):
            with self.subTest(compiler=name):
                binary = self.root / name
                info = subprocess.check_output(["xcrun", "vtool", "-show-build", str(binary)], text=True)
                self.assertRegex(info, r"minos\s+15\.1\b")
                subprocess.run([str(binary)], check=True, capture_output=True)
        # Tracing's helpers use the same shell authority, including Swift's
        # explicit target, rather than inheriting a compiler-specific default.
        compile_native = runpy.run_path(str(ROOT / "scripts/native-build.py"))["compile_native"]
        compile_native(self.root, "cc", str(self.root / "main.c"), "-o", str(self.root / "trace-c"))
        compile_native(self.root, "swiftc", "-module-cache-path", str(self.root / "module-cache"),
                       str(self.root / "main.swift"), "-o", str(self.root / "trace-swift"))
        for name in ("trace-c", "trace-swift"):
            info = subprocess.check_output(["xcrun", "vtool", "-show-build", str(self.root / name)], text=True)
            self.assertRegex(info, r"minos\s+15\.1\b")


if __name__ == "__main__":
    unittest.main()
