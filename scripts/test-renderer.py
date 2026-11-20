#!/usr/bin/env python3
"""Run Playbit's vendored-renderer regressions, excluding upstream unit tests."""
import subprocess


def main():
    cargo = ['cargo', 'test', '--locked', '-p', 'gpui', '--no-default-features',
             '--features', 'font-kit', '--lib', 'playbit_']
    listing = subprocess.run(cargo + ['--', '--list'], check=True, capture_output=True, text=True)
    tests = [line.removesuffix(': test') for line in listing.stdout.splitlines() if line.endswith(': test')]
    if not tests or any(not name.rsplit('::', 1)[-1].startswith('playbit_') for name in tests):
        raise SystemExit('expected a nonempty selection of Playbit renderer regressions')
    subprocess.run(cargo, check=True)


if __name__ == '__main__':
    main()
