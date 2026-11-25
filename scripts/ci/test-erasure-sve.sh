#!/usr/bin/env bash
# Exercise all legal SVE lengths, both SVE kernels and the NEON fallback.
# Requires rustup target add aarch64-unknown-linux-musl and qemu-aarch64.
set -euo pipefail
root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
python3 - "$root" "$tmp" <<'PY'
import json
import sys
from pathlib import Path
root, tmp = map(Path, sys.argv[1:])
path = json.dumps(str(root / "src/data/ec/mod.rs"))
(tmp / "harness.rs").write_text(
    '#![allow(dead_code)]\nmod data { #[path = ' + path + '] mod ec; }\n'
)
PY
rustc --target aarch64-unknown-linux-musl --edition=2021 --test \
  -C opt-level=3 -C linker=rust-lld "$tmp/harness.rs" -o "$tmp/ec-tests"
qemu="${QEMU_AARCH64:-qemu-aarch64}"
RS_REQUIRE_KERNELS=neon,scalar "$qemu" -cpu cortex-a72 "$tmp/ec-tests" data::ec::kernel::
for vl in 16 32 48 64 80 96 112 128 144 160 176 192 208 224 240 256; do
  echo "SVE/SVE2 vector length: $vl bytes"
  RS_REQUIRE_KERNELS=neon,sve,sve2,scalar RS_REQUIRE_SVE_BYTES="$vl" \
    "$qemu" -cpu "max,sve-default-vector-length=$vl" "$tmp/ec-tests" data::ec::kernel::
  RS_REQUIRE_KERNELS=neon,sve,sve2,scalar \
    "$qemu" -cpu "max,sve-default-vector-length=$vl" "$tmp/ec-tests" data::ec::driver::tests::fused_recovery
done
