#!/usr/bin/env python3
"""Differential memory/skill/queue/delegation/calendar tests using isolated databases and no models/credentials."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def run(command, directory):
    env = dict(os.environ, HIVE_HOME=str(directory / 'home'), HIVE_DB_PATH=str(directory / 'db'), HIVE_EMBEDDER='off')
    result = subprocess.run(command, cwd=ROOT, env=env, text=True, capture_output=True, timeout=30, check=True)
    lines = [line.removeprefix('PARITY_RESULT:') for line in result.stdout.splitlines() if line.startswith('PARITY_RESULT:')]
    if len(lines) != 1:
        raise RuntimeError('Expected exactly one parity result; no secrets or unrelated stdout are included in the report')
    return json.loads(lines[0])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=ROOT.parent.parent / 'hive-sdk')
    parser.add_argument('--node', default='node')
    parser.add_argument('--bun', default='bun')
    parser.add_argument('--scenario', choices=['memory', 'skills', 'queue', 'delegation', 'scheduler', 'all'], default='all')
    args = parser.parse_args()
    subprocess.run(['python3', str(ROOT / 'scripts/snapshot-ts-reference.py'), '--source', str(args.source), '--check'], check=True)
    scenarios = ['memory', 'skills', 'queue', 'delegation', 'scheduler'] if args.scenario == 'all' else [args.scenario]
    for scenario in scenarios:
        with tempfile.TemporaryDirectory(prefix='hive-ts-parity-') as ts_dir, tempfile.TemporaryDirectory(prefix='hive-rust-parity-') as rust_dir:
            ts = run([args.bun, str(ROOT / f'tests/parity/{scenario}-ts.ts'), str(args.source.resolve())], Path(ts_dir))
            rust = run([args.node, str(ROOT / f'tests/parity/{scenario}-rust.mjs')], Path(rust_dir))
        expected_rust = ts
        if scenario == 'memory':
            expected = {'a': 'Viernes', 'b': 'Lunes', 'list': ['Entrega'], 'search': ['Entrega'], 'absent': True, 'recreated': 'Sábado'}
            assert ts == expected, ('TS memory baseline differs', ts)
        elif scenario == 'skills':
            assert ts['name'] == 'resumir_texto' and len(ts['steps']) == 1 and '🦀' in ts['content'], ('TS skill baseline differs', ts)
        elif scenario == 'scheduler':
            expected = ['2026-11-16T13:00:00.000Z', '2026-01-01T00:00:15.000Z', '2026-03-09T06:30:00.000Z',
                        '2104-02-29T00:00:00.000Z', '2026-11-02T09:00:00.000Z', '2027-02-15T09:00:00.000Z', None,
                        '2026-01-04T08:00:00.000Z', '2026-01-04T00:00:00.000Z', '2026-11-02T00:00:00.000Z']
            assert ts == expected, ('TS calendar baseline differs', ts)
            expected_rust = ts[:-1] + ['2026-11-03T00:00:00.000Z']
        elif scenario == 'delegation':
            expected = {'waitsForAll': True, 'summaryCount': 1, 'outcomeCount': 2, 'failurePreserved': True, 'uncheckedVisible': True}
            assert ts == expected, ('TS delegation baseline differs', ts)
        else:
            expected = {'deduplicated': True, 'attempts': 1, 'duplicateClaim': True, 'status': 'completed', 'receipt': 'one', 'cancelled': 'cancelled'}
            assert ts == expected, ('TS queue baseline differs', ts)
        assert rust == expected_rust, (f'{scenario} parity differs', rust, expected_rust)
        print(json.dumps({'scenario': scenario, 'passed': True,
                          'known_differences': ['Native cron honors wildcard day steps; pinned TS ignores that restriction'] if scenario == 'scheduler' else [],
                          'limits': 'Selected memory/skill/job-store/group/calendar outcomes only; not full API, ranking, scheduling, recovery or shutdown parity'}, ensure_ascii=False))



if __name__ == '__main__':
    main()
