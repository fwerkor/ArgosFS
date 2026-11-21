#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
out="${1:-${ARGOSFS_PJDFSTEST_OUT:-target/argosfs-artifacts/compat/pjdfstest.jsonl}}"
build_log="${out%.jsonl}.build.log"

if [ ! -e /dev/fuse ]; then
  echo "ERROR: /dev/fuse is required for mandatory pjdfstest coverage" >&2
  exit 1
fi
if [ -z "${PJDFSTEST_ROOT:-}" ] || [ ! -d "$PJDFSTEST_ROOT/tests" ]; then
  echo "ERROR: PJDFSTEST_ROOT must point to a checked-out pjdfstest tree" >&2
  exit 1
fi

mkdir -p "$(dirname "$out")"
if [ ! -x "$PJDFSTEST_ROOT/pjdfstest" ]; then
  for command in autoreconf make; do
    if ! command -v "$command" >/dev/null 2>&1; then
      echo "ERROR: $command is required to build pjdfstest" >&2
      exit 1
    fi
  done
  if ! (
    cd "$PJDFSTEST_ROOT"
    autoreconf -ifs
    ./configure
    make -j"$(nproc)" pjdfstest
  ) >"$build_log" 2>&1; then
    echo "ERROR: failed to build pjdfstest; log=$build_log" >&2
    tail -n 200 "$build_log" >&2 || true
    exit 1
  fi
fi
if [ ! -x "$PJDFSTEST_ROOT/pjdfstest" ]; then
  echo "ERROR: pjdfstest executable is missing after build" >&2
  exit 1
fi

if [ -w /etc/fuse.conf ]; then
  grep -qxF user_allow_other /etc/fuse.conf || printf '%s\n' user_allow_other >>/etc/fuse.conf
else
  sudo sh -c 'grep -qxF user_allow_other /etc/fuse.conf 2>/dev/null || printf "%s\n" user_allow_other >>/etc/fuse.conf'
fi

export ARGOSFS_COMPAT_MOUNT_OPTIONS="allow_other,default_permissions"
export ARGOSFS_PJDFSTEST_REQUIRE=1
export ARGOSFS_PJDFSTEST_RUN_AS_ROOT=1

# Each pjdfstest file uses the mount root as its working directory. Separate
# mounts (not prove -j on one mount) prevent concurrent suites from clobbering
# one another's paths or changing their results.
jobs="${ARGOSFS_PJDFSTEST_JOBS:-1}"
if [[ ! "$jobs" =~ ^[1-8]$ ]]; then
  echo "ERROR: ARGOSFS_PJDFSTEST_JOBS must be an integer from 1 to 8" >&2
  exit 2
fi
if (( jobs == 1 )); then
  exec "$repo/scripts/compat/run_pjdfstest.sh" "$out"
fi

requested="${ARGOSFS_PJDFSTEST_TESTS:-chmod chown link mkdir open rename rmdir symlink truncate unlink utimensat}"
selected=()
for test_name in $requested; do
  if [ -f "$PJDFSTEST_ROOT/tests/$test_name" ]; then
    selected+=("$test_name")
  fi
done
if (( ${#selected[@]} == 0 )); then
  echo "ERROR: no requested pjdfstest suites found" >&2
  exit 1
fi
if (( jobs > ${#selected[@]} )); then
  jobs=${#selected[@]}
fi

# Warm the release build before starting mounts. Otherwise all workers run
# cargo build simultaneously and contend on Cargo's target directory lock.
cargo build --release >/dev/null

work="${ARGOSFS_COMPAT_WORKDIR:-${out%.jsonl}-work}"
mkdir -p "$work"
shards=()
pids=()
for ((i = 0; i < jobs; i++)); do
  shards+=("")
done
for ((i = 0; i < ${#selected[@]}; i++)); do
  shard=$((i % jobs))
  shards[$shard]="${shards[$shard]} ${selected[$i]}"
done

for ((i = 0; i < jobs; i++)); do
  worker_out="${out%.jsonl}-shard-$((i + 1)).jsonl"
  echo "pjdfstest shard $((i + 1))/$jobs:${shards[$i]}"
  (
    export ARGOSFS_PJDFSTEST_TESTS="${shards[$i]}"
    export ARGOSFS_COMPAT_WORKDIR="$work/shard-$((i + 1))"
    "$repo/scripts/compat/run_pjdfstest.sh" "$worker_out"
  ) >"${worker_out%.jsonl}.runner.log" 2>&1 &
  pids+=("$!")
done

failed=0
# Wait for every worker even when another fails: all mounts must unmount and
# flush their instrumented LLVM profiles before the aggregate coverage report.
for ((i = 0; i < jobs; i++)); do
  worker_out="${out%.jsonl}-shard-$((i + 1)).jsonl"
  if ! wait "${pids[$i]}" || ! grep -q '"suite":"pjdfstest","status":"passed"' "$worker_out" 2>/dev/null; then
    echo "ERROR: pjdfstest shard $((i + 1)) failed; logs: ${worker_out%.jsonl}.runner.log" >&2
    tail -n 60 "${worker_out%.jsonl}.runner.log" >&2 || true
    failed=1
  fi
done
if (( failed != 0 )); then
  printf '{"suite":"pjdfstest","status":"failed","reason":"parallel shard failure"}\n' >"$out"
  exit 1
fi
printf '{"suite":"pjdfstest","status":"passed","reason":"%d isolated parallel shards"}\n' "$jobs" >"$out"
cat "$out"
