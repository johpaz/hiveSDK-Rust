#!/usr/bin/env bash
# Run inside a pypa manylinux/musllinux image, with this source tree at /io.
set -euo pipefail
cd /io
target="${1:?Rust target required}"
compatibility="${2:?Wheel compatibility tag required}"
export PATH="/root/.cargo/bin:$PATH"
if ! command -v rustup >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/hive-rustup.sh
  sh /tmp/hive-rustup.sh -y --profile minimal --default-toolchain 1.100.0
fi
rustup toolchain install 1.100.0 --profile minimal
rustup default 1.100.0
rustup target add "$target"
export PYO3_PYTHON=/opt/python/cp310-cp310/bin/python
if [[ "$target" == *musl ]]; then
  export RUSTFLAGS='-C target-feature=-crt-static'
fi
cargo build --release --locked -p hive-agent-napi --target "$target"
"$PYO3_PYTHON" -m pip install 'maturin==1.16.0'
cd packages/python
"$PYO3_PYTHON" -m maturin build --release --locked --target "$target" --compatibility "$compatibility" --out ../../dist/wheels
