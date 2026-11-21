## Summary

Describe what changed and why. Keep unrelated changes in separate PRs.

## Related issue

Related: <!-- Link the issue and use "Fixes #123" when this PR resolves it. -->

## Validation

List commands actually run and their outcomes. Mark unavailable or
non-applicable checks explicitly; do not mark skips as passes.

- [ ] `cargo fmt --package argosfs -- --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --all-targets --all-features -- --test-threads=1`
- [ ] Relevant host/FUSE/QEMU/fault-injection checks (list below or explain N/A)

Commands, results, and environmental limitations:

## Storage safety and compatibility

- [ ] Considered data integrity, crash recovery, and degraded/missing-shard behavior (or N/A)
- [ ] Considered format/metadata compatibility and upgrade impact (or N/A)
- [ ] Added or updated regression tests for behavioral changes (or explained why not)
- [ ] Updated documentation for user-facing changes (or N/A)

For performance changes, include a reproducible baseline, workload, and
before/after measurements. For docs-only changes, explain why runtime checks
are not applicable.

See [CONTRIBUTING.md](https://github.com/fwerkor/ArgosFS/blob/main/CONTRIBUTING.md) for the full contribution guide.
