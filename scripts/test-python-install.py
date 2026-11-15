"""Install one prebuilt wheel in a clean venv and hide build tools from its PATH."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
if len(sys.argv) != 2:
    raise SystemExit('Pass exactly one prebuilt wheel')
wheel = Path(sys.argv[1]).resolve(strict=True)
with tempfile.TemporaryDirectory(prefix='hive-python-consumer-') as directory:
    subprocess.run([sys.executable, '-m', 'venv', directory], check=True)
    binary_dir = Path(directory) / ('Scripts' if os.name == 'nt' else 'bin')
    python = binary_dir / ('python.exe' if os.name == 'nt' else 'python')
    env = dict(os.environ, PATH=str(binary_dir), PIP_DISABLE_PIP_VERSION_CHECK='1')
    env.pop('PYTHONPATH', None)
    subprocess.run([str(python), '-m', 'pip', 'install', '--no-index', '--no-deps', str(wheel)], env=env, check=True)
    subprocess.run([str(python), '-I', str(root / 'tests/installed-smoke.py')], env=env, check=True)
