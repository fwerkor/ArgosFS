#!/usr/bin/env bash
set -euo pipefail

target="${1:?usage: run-fuzz.sh TARGET [SECONDS] [SEED] [CORPUS_DIR]}"
seconds="${2:-60}"
seed="${3:-1}"
corpus="${4:-fuzz/corpus/$target}"
artifacts="${ARGOSFS_FUZZ_ARTIFACTS:-fuzz/artifacts/$target}"

case "$seconds" in
  ''|*[!0-9]*|0) echo "SECONDS must be a positive integer" >&2; exit 2 ;;
esac
case "$seed" in
  ''|*[!0-9]*) echo "SEED must be an integer" >&2; exit 2 ;;
esac

mkdir -p "$corpus" "$artifacts"
exec cargo fuzz run "$target" "$corpus" -- \
  "-max_total_time=$seconds" \
  "-timeout=${ARGOSFS_FUZZ_CASE_TIMEOUT:-10}" \
  "-rss_limit_mb=${ARGOSFS_FUZZ_RSS_MB:-4096}" \
  "-max_len=${ARGOSFS_FUZZ_MAX_LEN:-8192}" \
  "-seed=$seed" \
  "-artifact_prefix=$artifacts/"
