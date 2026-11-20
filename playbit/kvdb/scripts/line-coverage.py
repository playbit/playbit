#!/usr/bin/env python3
"""Gate on line coverage as the annotated report shows it.

`cargo llvm-cov --text` annotates every line with its execution count. This
counts a line as covered when any monomorphization or closure executed it,
which is what a reader of the report sees. llvm-cov's own summary credits a
generic function with only its best single instantiation, so error paths
exercised through one resolver or closure type do not count for another
and the summary understates coverage of generic code.

usage: line-coverage.py <report.txt> <minimum-percent>
"""
import re
import sys

report, minimum = sys.argv[1], float(sys.argv[2])
current = None
lines = {}
with open(report) as source:
    for text in source:
        header = re.match(r"^(\S+\.rs):$", text)
        if header:
            current = header.group(1)
            lines.setdefault(current, [])
            continue
        annotated = re.match(r"^\s*(\d+)\|\s*([0-9.]+[kMG]?)\|", text)
        if annotated and current:
            lines[current].append((int(annotated.group(1)), annotated.group(2) != "0"))

total = covered = 0
for path in sorted(lines):
    counted = lines[path]
    missed = [number for number, hit in counted if not hit]
    total += len(counted)
    covered += len(counted) - len(missed)
    if missed:
        print(f"{path}: {len(missed)} uncovered: {' '.join(map(str, missed))}")
if total == 0:
    sys.exit("no annotated lines found in the report")
percent = 100.0 * covered / total
print(f"line coverage {covered}/{total} = {percent:.2f}% (minimum {minimum:g}%)")
sys.exit(0 if percent >= minimum else 1)
