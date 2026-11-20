#!/usr/bin/env python3
# Python matches the existing CI scripts in scripts/: this is a report, not a
# product path. Use only the standard library and brace matching, not a Rust parser.
"""Report source-level Tiger Style metrics; gate only the assigned tier A scopes.

Lines include comments and blanks. Token counts ignore comments and literals.
Functions include declarations and extern functions; lengths start at the fn line.
This is a textual approximation, not macro expansion or cfg evaluation. Only
cfg(test) modules (including their declarations) and named test/benchmark files
are excluded; other conditional items remain part of the source measurement.
"""
import argparse
from collections import Counter
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
DIRECTORIES = [
    'playbit/src/' + name for name in
    ('runtime', 'store', 'engine', 'network', 'desktop', 'platform', 'package', 'bin')
] + ['playbit/kvdb/src', 'playbit/trace/src', 'playbit-api/src']
TIER_A = [
    'playbit/src/desktop/update/' + name for name in ('archive.rs', 'package.rs', 'install.rs')
] + [
    'playbit/src/' + name for name in ('runtime', 'store', 'network', 'package', 'platform/web')
] + ['playbit/kvdb/src', 'playbit-api/src']
EXCLUDED_DIRS = {'vendor', 'target', 'node_modules', 'tests', 'benches', 'benchmarks'}
LITERALS = re.compile(
    r'//[^\n]*|/\*.*?\*/|(?:br|cr|r)(?P<hashes>\#*)".*?"(?P=hashes)'
    r'|(?:b|c)?"(?:\\.|[^"\\])*"|b?\'(?:\\.|[^\'\\\n])\'', re.DOTALL
)
FUNCTION = re.compile(
    r'^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|const|unsafe|extern)\s+)*fn\s+(\w+)'
)
ASSERTION = re.compile(r'\b(?:assert(?:_eq|_ne)?|debug_assert\w*|unreachable)\s*!')


def mask_literals(source):
    """Leave line/column positions intact for brace matching and diagnostics."""
    return LITERALS.sub(lambda m: re.sub(r'[^\n]', ' ', m.group()), source)


def production_file(path):
    return (path.suffix == '.rs' and not EXCLUDED_DIRS.intersection(path.parts)
            and path.name not in {'tests.rs', 'test_support.rs', 'bench.rs', 'benchmark.rs'}
            and not path.stem.endswith(('_tests', '_bench', '_benches', '_benchmark', '_benchmarks')))


def production_lines(source):
    raw = source.splitlines()
    code = mask_literals(source).splitlines()
    excluded = set()
    for start, line in enumerate(code):
        if not re.fullmatch(r'\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*', line):
            continue
        pos = start + 1
        while pos < len(code):
            if not code[pos].strip():
                pos += 1
                continue
            if not code[pos].lstrip().startswith('#['):
                break
            # rustfmt can wrap an attribute between cfg(test) and its module.
            # Literals are already masked, so only attribute brackets count.
            depth = code[pos].count('[') - code[pos].count(']')
            pos += 1
            while pos < len(code) and depth:
                depth += code[pos].count('[') - code[pos].count(']')
                pos += 1
        if pos == len(code) or not re.match(r'\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+', code[pos]):
            continue
        depth = 0
        opened = False
        for end in range(pos, len(code)):
            depth += code[end].count('{') - code[end].count('}')
            opened |= '{' in code[end]
            if (opened and depth == 0) or (not opened and ';' in code[end]):
                excluded.update(range(start, end + 1))
                break
    return [(i + 1, raw[i], code[i]) for i in range(len(raw)) if i not in excluded]


def functions(lines, path):
    result = []
    occurrences = Counter()
    for pos, (number, raw, code) in enumerate(lines):
        match = FUNCTION.match(code)
        if not match:
            continue
        name = match[1]
        occurrences[name] += 1
        # Ordinals distinguish repeated method names without making line shifts
        # into new functions. The report displays path::name and a source line.
        identity = f'{path}::{name}#{occurrences[name]}'
        depth = 0
        opened = False
        end = number
        signature_depth = 0
        for end, _, body in lines[pos:]:
            for token in body[match.end():] if end == number else body:
                if not opened and token in '([':
                    signature_depth += 1
                elif not opened and token in ')]':
                    signature_depth -= 1
                if token == '{':
                    depth += 1
                    opened = True
                elif token == '}':
                    depth -= 1
                if (opened and depth == 0) or (not opened and token == ';' and signature_depth == 0):
                    break
            else:
                continue
            break
        annotation = re.search(r'//\s*fuzz:\s*(\S+)', lines[pos - 1][1]) if pos else None
        result.append({'id': identity, 'path': path, 'name': name, 'line': number,
                       'lines': end - number + 1,
                       'fuzz_annotation': annotation[1] if annotation else None})
    return result


def measure(files):
    totals = dict.fromkeys(('lines', 'functions', 'assertions', 'unwrap', 'expect',
                           'limit_constants', 'unsafe', 'lines_over_100'), 0)
    found = []
    for path, lines in files:
        code = '\n'.join(line[2] for line in lines)
        items = functions(lines, path)
        found.extend(items)
        totals['lines'] += len(lines)
        totals['functions'] += len(items)
        totals['assertions'] += len(ASSERTION.findall(code))
        for method in ('unwrap', 'expect'):
            totals[method] += len(re.findall(r'\.\s*' + method + r'\s*\(', code))
        totals['limit_constants'] += sum(
            any(word in name for word in ('MAX', 'LIMIT', 'CAP', 'BUDGET'))
            for name in re.findall(r'\bconst\s+(\w+)\s*:', code))
        totals['unsafe'] += len(re.findall(r'\bunsafe\b', code))
        totals['lines_over_100'] += sum(len(raw.expandtabs(4)) > 100 for _, raw, _ in lines)
    totals['unwrap_expect'] = totals['unwrap'] + totals['expect']
    totals['assertions_per_function'] = totals['assertions'] / totals['functions'] if totals['functions'] else 0
    totals['long_functions'] = sorted(
        (item for item in found if item['lines'] > 70), key=lambda f: (-f['lines'], f['id']))
    return totals, found


def in_scope(path, scope):
    return path == scope or path.startswith(scope + '/')


def crate_counts(root):
    commands = {
        'playbit (desktop)': ['-p', 'playbit'],
        'playbit (no default features)': ['-p', 'playbit', '--no-default-features'],
        'kvdb (redb)': ['-p', 'kvdb', '--features', 'redb'],
    }
    results = {}
    for name, args in commands.items():
        command = ['cargo', 'tree', '-e', 'normal', '--prefix', 'none', '--locked', *args]
        try:
            run = subprocess.run(command, cwd=root, capture_output=True, text=True, timeout=60)
            if run.returncode:
                raise ValueError(run.stderr.strip())
            names = set(re.findall(r'^([\w-]+) v\S+', run.stdout, re.MULTILINE))
            if not names:
                raise ValueError('cargo tree returned no crate names')
            results[name] = {'count': len(names), 'unavailable': None}
        except (OSError, ValueError, subprocess.TimeoutExpired) as error:
            results[name] = {'count': None, 'unavailable': str(error)}
    return results


def report(root):
    files = {}
    for directory in DIRECTORIES:
        for path in sorted((root / directory).rglob('*.rs')):
            relative = path.relative_to(root).as_posix()
            if production_file(Path(relative)):
                files[relative] = production_lines(path.read_text())
    directories = {}
    for directory in DIRECTORIES:
        directories[directory], _ = measure((p, s) for p, s in files.items() if in_scope(p, directory))
    tiers = {}
    decoders = []
    targets = {
        p.relative_to(root).as_posix(): p.read_text()
        for p in sorted((root / 'playbit/fuzz/fuzz_targets').glob('*.rs'))
    }
    for scope in TIER_A:
        tiers[scope], items = measure((p, s) for p, s in files.items() if in_scope(p, scope))
        for item in items:
            if not item['name'].startswith(('decode', 'parse', 'validate')):
                continue
            hits = [p for p, source in targets.items() if item['name'] in source]
            decoders.append({**item, 'fuzz_targets': hits,
                             'covered': bool(hits or item['fuzz_annotation'])})
    return {'directories': directories, 'tier_a': tiers, 'decoders': decoders,
            'crates': crate_counts(root)}


def violations(current, baseline):
    errors = []
    for scope, now in current['tier_a'].items():
        old = baseline['tier_a'].get(scope)
        if old is None:
            errors.append(f'{scope}: baseline scope missing; baseline=null current=present')
            continue
        if now['unwrap'] > old['unwrap']:
            errors.append(f"{scope}: unwrap baseline={old['unwrap']} current={now['unwrap']}")
        known = {f['id'] for f in old['long_functions']}
        for function in now['long_functions']:
            if function['id'] not in known:
                errors.append(f"{scope}: {function['path']}::{function['name']}:{function['line']}: long_function baseline=absent current={function['lines']} lines")
    return errors


def markdown(data):
    out = ['# Tiger Style report', '',
           'Production Rust source; comments and blank lines count toward lengths. '
           'Token counts exclude comments and literals. Functions include declarations and extern functions. '
           'Test modules and named test/benchmark files are excluded; other cfg items are retained.', '',
           '| Directory | Lines | Functions | Assertions | Per function | `unwrap`/`expect` |',
           '|---|---:|---:|---:|---:|---:|']
    for path, m in data['directories'].items():
        out.append(f"| {path} | {m['lines']:,} | {m['functions']} | {m['assertions']} | {m['assertions_per_function']:.2f} | {m['unwrap_expect']} |")
    out += ['', '| Directory | >70 lines | Limits | Unsafe | >100 columns |', '|---|---:|---:|---:|---:|']
    for path, m in data['directories'].items():
        out.append(f"| {path} | {len(m['long_functions'])} | {m['limit_constants']} | {m['unsafe']} | {m['lines_over_100']} |")
    for path, m in data['directories'].items():
        out += ['', f'## {path}: ten largest functions over 70 lines', '', '| Function | Lines |', '|---|---:|']
        for f in m['long_functions'][:10]:
            out.append(f"| `{f['path']}::{f['name']}` (line {f['line']}) | {f['lines']} |")
        if len(m['long_functions']) > 10:
            out += ['', '<details><summary>Remaining functions over 70 lines</summary>', '', '| Function | Lines |', '|---|---:|']
            for f in m['long_functions'][10:]:
                out.append(f"| `{f['path']}::{f['name']}` (line {f['line']}) | {f['lines']} |")
            out += ['', '</details>']
        if not m['long_functions']:
            out.append('| None | — |')
    out += ['', '## Tier A decoder inventory', '',
            'Coverage means name present in target source or an immediately preceding `// fuzz: <target>` comment; it is not proof of execution.', '',
            '| Function | Covered | Target or annotation |', '|---|---|---|']
    for f in data['decoders']:
        targets = ', '.join(f['fuzz_targets'])
        if f['fuzz_annotation']:
            targets += ('; ' if targets else '') + '// fuzz: ' + f['fuzz_annotation']
        out.append(f"| `{f['path']}::{f['name']}` (line {f['line']}) | {'yes' if f['covered'] else 'no'} | {targets or '—'} |")
    out += ['', '## Transitive crates', '', 'Unique crate names from `cargo tree -e normal --prefix none --locked`, including the root crate; host-dependent.', '',
            '| Build | Crates |', '|---|---:|']
    for name, value in data['crates'].items():
        out.append(f"| {name} | {value['count'] if value['count'] is not None else 'null (cargo tree unavailable)'} |")
        if value['unavailable']:
            reason = value['unavailable'].replace('\n', ' ').replace('|', '\\|')
            out.append(f'\n{name}: cargo tree unavailable: {reason}\n')
    return '\n'.join(out) + '\n'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--json', type=Path, help='write the measurements as JSON')
    parser.add_argument('--check', action='store_true', help='fail on tier A regressions against the committed baseline')
    args = parser.parse_args()
    data = report(ROOT)
    errors = []
    if args.check:
        baseline = json.loads((ROOT / 'scripts/style-baseline.json').read_text())
        errors = violations(data, baseline)
    if args.json:
        args.json.write_text(json.dumps(data, indent=2) + '\n')
    print(markdown(data), end='')
    if args.check:
        print('\n## Baseline check\n')
        print('\n'.join('- ' + error for error in errors) if errors else 'PASS: no tier A regressions.')
    return bool(errors)


if __name__ == '__main__':
    sys.exit(main())
