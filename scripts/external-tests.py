#!/usr/bin/env python3
"""Runs the opt-in checks that need something outside the repository (a display, Ollama, LibreOffice…)
and writes `external-report.json`, a report that is safe to send back: it holds the platform, tool
versions, what ran and how it ended, and the tail of each run's output. It never includes
environment variables, keys or tokens.

  python3 scripts/external-tests.py list
  python3 scripts/external-tests.py run rust-suite libreoffice
  python3 scripts/external-tests.py run x11 --x11-display :9
  python3 scripts/external-tests.py run ollama --ollama-model qwen3.5:4b

An area that cannot run here is reported as `skipped` with the reason, never as a pass.
"""
import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TAIL = 4000


def run(cmd, env=None, timeout=3600):
    started = time.time()
    try:
        done = subprocess.run(cmd, cwd=ROOT, env={**os.environ, **(env or {})}, capture_output=True, text=True, timeout=timeout)
        output = (done.stdout + done.stderr)[-TAIL:]
        return {'status': 'passed' if done.returncode == 0 else 'failed', 'seconds': round(time.time() - started, 1), 'output_tail': output}
    except FileNotFoundError as error:
        return {'status': 'skipped', 'reason': f'{cmd[0]} is not installed ({error.filename})'}
    except subprocess.TimeoutExpired:
        return {'status': 'failed', 'seconds': timeout, 'output_tail': 'timed out'}


def cargo_test(*extra, env=None):
    return run(['cargo', 'test', '--locked', *extra], env)


def area_rust_suite(args):
    return cargo_test('--workspace', '--all-features')


def area_x11(args):
    if not args.x11_display: return {'status': 'skipped', 'reason': 'pass --x11-display :N, a DISPOSABLE X display (for example a nested Xwayland :9): the test moves its pointer and types into it'}
    return cargo_test('-p', 'hive-agent-core', '--features', 'computer-use', '--test', 'computer_x11', env={'HIVE_TEST_X11_DISPLAY': args.x11_display})


def area_portal_fake(args):
    if not shutil.which('dbus-daemon'): return {'status': 'skipped', 'reason': 'dbus-daemon is not installed'}
    return cargo_test('-p', 'hive-agent-core', '--features', 'computer-use', '--test', 'computer_portal')


def area_ollama(args):
    if not args.ollama_model: return {'status': 'skipped', 'reason': 'pass --ollama-model NAME, a LOCAL model with tool support (not a :cloud one, which would send prompts to a remote service)'}
    env = {'HIVE_TEST_OLLAMA_MODEL': args.ollama_model}
    if args.ollama_url: env['HIVE_TEST_OLLAMA_URL'] = args.ollama_url
    return cargo_test('-p', 'hive-agent-core', '--all-features', '--test', 'ollama_live', env=env)


def area_libreoffice(args):
    soffice = shutil.which('soffice') or shutil.which('libreoffice')
    if not soffice: return {'status': 'skipped', 'reason': 'LibreOffice (soffice) is not installed'}
    if not shutil.which('pdftotext'): return {'status': 'skipped', 'reason': 'pdftotext (poppler) is not installed'}
    return cargo_test('-p', 'hive-agent-core', '--all-features', '--test', 'office', env={'HIVE_TEST_SOFFICE': soffice})


def area_office_python(args):
    if not args.office_python: return {'status': 'skipped', 'reason': 'pass --office-python PATH, a python with python-docx, openpyxl and python-pptx'}
    return cargo_test('-p', 'hive-agent-core', '--all-features', '--test', 'office', env={'HIVE_OFFICE_PYTHON': args.office_python})


def area_bindings(args):
    results = {}
    if not shutil.which('python3.10') and not shutil.which('python3'): return {'status': 'skipped', 'reason': 'python is not installed'}
    py = shutil.which('python3.10') or shutil.which('python3')
    results['build'] = run([py, 'scripts/build-dev.py'])
    if shutil.which('node'): results['npm'] = run(['node', '--test', *map(str, sorted((ROOT / 'packages/npm/test').glob('*.test.mjs')))])
    results['python'] = run([py, '-m', 'unittest', 'discover', 'packages/python/tests'], env={'PYTHONPATH': str(ROOT / 'packages/python/python')})
    failed = [k for k, v in results.items() if v['status'] == 'failed']
    return {'status': 'failed' if failed else 'passed', 'parts': results}


AREAS = {
    'rust-suite': (area_rust_suite, 'the whole Rust test suite (every feature)'),
    'bindings': (area_bindings, 'the npm and Python packages built from this tree'),
    'x11': (area_x11, 'the X11 desktop backend against a disposable X server'),
    'portal-fake': (area_portal_fake, 'the Wayland portal backend against a fake portal on a private D-Bus'),
    'ollama': (area_ollama, 'the native Ollama adapter against a real local Ollama'),
    'libreoffice': (area_libreoffice, 'generated Excel/Word/PowerPoint files laid out by LibreOffice'),
    'office-python': (area_office_python, 'generated Office files opened by python-docx/openpyxl/python-pptx'),
}


def versions():
    def first_line(cmd):
        try: return subprocess.run(cmd, capture_output=True, text=True, timeout=20).stdout.strip().splitlines()[0]
        except Exception: return None
    return {'platform': platform.platform(), 'machine': platform.machine(), 'python': sys.version.split()[0],
            'rustc': first_line(['rustc', '--version']), 'cargo': first_line(['cargo', '--version']),
            'node': first_line(['node', '--version']), 'session': os.environ.get('XDG_SESSION_TYPE')}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('command', choices=['list', 'run'])
    parser.add_argument('areas', nargs='*')
    parser.add_argument('--x11-display'); parser.add_argument('--ollama-model'); parser.add_argument('--ollama-url')
    parser.add_argument('--office-python'); parser.add_argument('--report', default='external-report.json')
    args = parser.parse_args()
    if args.command == 'list':
        for name, (_, what) in AREAS.items(): print(f'{name:14} {what}')
        return 0
    chosen = args.areas or list(AREAS)
    unknown = [a for a in chosen if a not in AREAS]
    if unknown: parser.error(f'unknown area(s): {", ".join(unknown)}')
    report = {'environment': versions(), 'results': {}}
    for name in chosen:
        print(f'== {name}', flush=True)
        report['results'][name] = AREAS[name][0](args)
        print(f'   {report["results"][name]["status"]}' + (f': {report["results"][name].get("reason")}' if report['results'][name]['status'] == 'skipped' else ''), flush=True)
    Path(args.report).write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')
    print(f'report written to {args.report}')
    return 1 if any(r['status'] == 'failed' for r in report['results'].values()) else 0


if __name__ == '__main__':
    sys.exit(main())
