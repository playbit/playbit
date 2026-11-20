"""Compile native diagnostic helpers with the checkout's canonical macOS target."""
import os
import platform
import subprocess
import sys


def compile_native(repo, compiler, *arguments):
    if sys.platform != "darwin" or compiler not in ("cc", "clang", "swiftc"):
        raise RuntimeError("native diagnostic compilation requires macOS and a supported compiler")
    minimum = subprocess.check_output(
        ["sh", "-ec", '. scripts/macos-env.sh; printf "%s\\n" "$MACOSX_DEPLOYMENT_TARGET"'],
        cwd=repo, text=True).strip()
    command = [compiler]
    if compiler == "swiftc":
        command += ["-target", f"{platform.machine()}-apple-macosx{minimum}"]
    subprocess.run(command + list(arguments),
                   env=dict(os.environ, MACOSX_DEPLOYMENT_TARGET=minimum), check=True)
