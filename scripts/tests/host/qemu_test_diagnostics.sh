#!/usr/bin/env bash

argosfs_qemu_test_cleanup() {
  local status="$1"
  local tmp="$2"
  local test_name="$3"
  local file relative destination

  if [ "$status" -ne 0 ]; then
    echo "QEMU host test $test_name failed (exit $status)" >&2
    destination="${ARGOSFS_QEMU_TEST_DIAGNOSTICS_DIR:-}"
    if [ -n "$destination" ]; then
      destination="$destination/$test_name"
      mkdir -p "$destination" || destination=""
    fi

    while IFS= read -r -d '' file; do
      relative="${file#"$tmp"/}"
      if [ -n "$destination" ]; then
        mkdir -p "$destination/$(dirname "$relative")" &&
          cp -- "$file" "$destination/$relative" || true
      fi
      case "$file" in
        *.log|*.out|*.status|*/retry-output)
          printf '\n--- %s ---\n' "$relative" >&2
          tail -n 100 "$file" >&2 || true
          ;;
      esac
    done < <(find "$tmp" -type f -print0)
  fi
  rm -rf "$tmp"
  exit "$status"
}
