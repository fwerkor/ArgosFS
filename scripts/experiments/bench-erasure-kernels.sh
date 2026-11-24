#!/usr/bin/env bash
set -euo pipefail
root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
baseline="${1:-2a344164b4bb3df79e9851aa50a140cf4c1e07c9}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
git -C "$root" archive "$baseline" vendor/reed-solomon-engine/src | tar -x -C "$tmp"
rustc --edition=2024 --crate-name reed_solomon_engine --crate-type rlib -C opt-level=3 \
  --cfg 'feature="std"' --cfg 'feature="alloc"' \
  "$tmp/vendor/reed-solomon-engine/src/lib.rs" -o "$tmp/libreed_solomon_engine.rlib"
# Rust path attribute needs an absolute path; escape it as a Rust string.
python3 - "$root" "$tmp" <<'PYBENCH'
import json
import sys
from pathlib import Path
root, tmp = map(Path, sys.argv[1:])
source = (root / "scripts/experiments/erasure-kernels-bench.rs").read_text()
source = source.replace('"@EC_PATH@"', json.dumps(str(root / "src/data/ec/mod.rs")))
(tmp / "bench.rs").write_text(source)
PYBENCH
rustc --edition=2021 --test -C opt-level=3 -C codegen-units=1 \
  "$tmp/bench.rs" --extern "reed_solomon_engine=$tmp/libreed_solomon_engine.rlib" -o "$tmp/bench"
"$tmp/bench" --ignored --exact data::compare --nocapture
