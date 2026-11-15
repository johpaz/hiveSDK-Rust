#!/usr/bin/env python3
"""Build local debug bindings without npm/pip build dependencies."""
import os
from pathlib import Path
import shutil
import subprocess
import sys

root = Path(__file__).resolve().parent.parent
env = dict(os.environ, CARGO_TARGET_DIR=str(root / "target"), PYO3_PYTHON=sys.executable)
subprocess.run(["cargo", "build", "--workspace", "--locked"], cwd=root, env=env, check=True)

if sys.platform == "win32":
    node_lib, py_lib, py_ext = "hive_agent_napi.dll", "_native.dll", ".pyd"
elif sys.platform == "darwin":
    node_lib, py_lib, py_ext = "libhive_agent_napi.dylib", "lib_native.dylib", ".so"
else:
    node_lib, py_lib, py_ext = "libhive_agent_napi.so", "lib_native.so", ".so"

node_target = root / "packages/npm/native/hive.node"
node_target.parent.mkdir(parents=True, exist_ok=True)
shutil.copy2(root / "target/debug" / node_lib, node_target)
shutil.copy2(root / "target/debug" / py_lib, root / "packages/python/python/hive_sdk" / ("_native" + py_ext))
print("Built npm and Python development bindings.")
