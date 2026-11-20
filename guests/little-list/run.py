#!/usr/bin/env python3

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def main():
    guest = Path(__file__).resolve().parent
    pb = Path(shutil.which("pb") or guest.parents[1] / "target/debug/Playbit.app/Contents/MacOS/pb").resolve()
    if not os.access(pb, os.X_OK):
        sys.exit("Put Playbit's pb on PATH, or build target/debug/Playbit.app first.")

    work = guest / "target/run"
    work.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, CARGO_TARGET_DIR=str(guest / "target/build"),
               PLAYBIT_PROFILE=str(work / "profile"))

    print("Building Little List…", flush=True)
    build = json.loads(subprocess.check_output([str(pb), "build"], cwd=guest, env=env, text=True))

    saved_path = work / "session.json"
    session = json.loads(saved_path.read_text()) if saved_path.exists() else None
    if not session or session["resolved_build_id"] != build["build_id"]:
        args = [str(pb), "session", "create", build["bundle"]]
        if session:
            print("Opening the new build with your saved list…", flush=True)
            args.extend(["--root-volume", session["root_volume"]])

        # Save before launching so an interrupted join can reuse the same session.
        print(args)
        session = json.loads(subprocess.check_output(args, cwd=guest, env=env, text=True))

        temporary = saved_path.with_suffix(".json.tmp")
        temporary.write_text(json.dumps(session, indent=2) + "\n")
        temporary.replace(saved_path)

    print("Opening Little List…", flush=True)
    os.execve(pb, [str(pb), "session", "join", session["id"]], env)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        sys.exit(f"Little List: {error}")
