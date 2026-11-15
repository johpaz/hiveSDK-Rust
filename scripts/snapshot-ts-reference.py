#!/usr/bin/env python3
"""Capture the local TS reference without importing it or touching its database.

This records source identity and public entry points, not a parity verdict.
Tool names are candidates from literal declarations, not an executable registry.
"""
import argparse
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def snapshot(source):
    package = json.loads((source / 'package.json').read_text())
    entries = {}
    for name, target in package['exports'].items():
        if isinstance(target, str):
            path = source / target
            entries[name] = {'path': target, 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
    files = {}
    for path in sorted((source / 'packages/core/src').rglob('*')):
        if path.is_file() and path.suffix in {'.ts', '.md'}:
            files[path.relative_to(source).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
    identity = hashlib.sha256(json.dumps(files, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
    return {'schema_version': 1, 'name': package['name'], 'version': package['version'],
            'source_tree_sha256': identity, 'exports': entries, 'files': files,
            'exclusions': ['project management, project entities, project tools, project context, project scaffolding'],
            'policy': {'functional_parity': True, 'legacy_import_compatibility': False,
                       'legacy_data_migration': False, 'node_bun_sidecars': False}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=ROOT.parent.parent / 'hive-sdk')
    parser.add_argument('--output', type=Path, default=ROOT / 'docs/parity/ts-reference.json')
    parser.add_argument('--check', action='store_true', help='Verify that the reference has not changed')
    args = parser.parse_args()
    data = snapshot(args.source.resolve())
    if args.check:
        if json.loads(args.output.read_text()) != data:
            raise SystemExit('TS reference drifted; review the source changes before updating the baseline')
        print(f"TS reference unchanged: {data['name']} {data['version']} ({len(data['files'])} files)")
    else:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(data, indent=2, ensure_ascii=False) + '\n')
        print(f"Captured {len(data['exports'])} entry points and {len(data['files'])} source files")


if __name__ == '__main__':
    main()
