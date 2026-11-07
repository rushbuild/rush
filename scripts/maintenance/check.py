#!/usr/bin/env python3
"""Check component coverage and pinned Chores files without running task code."""

import argparse
import fnmatch
import hashlib
import json
from pathlib import Path
import subprocess


ROOT = Path(__file__).resolve().parents[2]


def entries():
    result = []
    current = None
    for line in (ROOT / 'MAINTAINERS').read_text().splitlines():
        if not line or line.startswith('#'):
            continue
        if len(line) > 2 and line[1:3] == ': ':
            if current is None:
                raise ValueError('field without component')
            current.setdefault(line[0], []).append(line[3:])
        else:
            current = {'name': line}
            result.append(current)
    for entry in result:
        if not all(entry.get(field) for field in ('M', 'A', 'F', 'T')):
            raise ValueError('incomplete component: ' + entry['name'])
    return result


def route(path, components):
    candidate = Path(path)
    if candidate.is_absolute() or '..' in candidate.parts:
        raise ValueError('use a repository-relative path')
    return [entry['name'] for entry in components
            if any(fnmatch.fnmatchcase(path, pattern) for pattern in entry['F'])]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--route', nargs='+')
    args = parser.parse_args()
    components = entries()
    if args.route:
        print(json.dumps({path: route(path, components) for path in args.route}, indent=2))
        return
    paths = subprocess.check_output(
        ['git', 'ls-files', '-z'], cwd=ROOT).decode().split('\0')
    paths = [path for path in paths if path]
    if not paths:
        raise ValueError('stage the candidate before checking coverage')
    for path in paths:
        owners = route(path, components)
        if not owners:
            raise ValueError('unowned path: ' + path)
        if path.startswith('crates/') and len(owners) < 2:
            raise ValueError('crate lacks a component specialist: ' + path)
    manifest = json.loads((ROOT / 'maintenance/vendor/chores/PIN.json').read_text())
    vendor = ROOT / 'maintenance/vendor/chores'
    actual_files = {path.relative_to(vendor).as_posix()
                    for path in vendor.rglob('*') if path.is_file()
                    and path.name != 'PIN.json' and '__pycache__' not in path.parts}
    if actual_files != set(manifest['sha256']):
        raise ValueError('Chores file inventory changed')
    for name, expected in manifest['sha256'].items():
        if hashlib.sha256((vendor / name).read_bytes()).hexdigest() != expected:
            raise ValueError('Chores pin changed: ' + name)
    print(f'PASS: {len(paths)} paths owned; {len(components)} components; Chores pin intact')


if __name__ == '__main__':
    main()
