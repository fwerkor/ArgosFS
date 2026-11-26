#!/usr/bin/env bash
# Convenience wrapper for the workspace-excluded ztest package.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec cargo run --manifest-path "$here/Cargo.toml" --release -- run "$@"
