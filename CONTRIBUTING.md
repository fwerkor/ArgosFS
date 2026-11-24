# Contributing to ArgosFS

Thank you for contributing to ArgosFS. Filesystem changes can affect data
integrity and bootability, so please prioritize reproducible changes and
regression coverage over large, unreviewable patches.

## Before you start

- For a bug, search [existing issues](https://github.com/fwerkor/ArgosFS/issues)
  and include a minimal reproducer. For a substantial feature or a change to
  on-disk formats, open an issue to discuss the approach first.
- For usage questions or early design ideas, use
  [GitHub Discussions](https://github.com/fwerkor/ArgosFS/discussions).
- **Do not open a public issue for a security vulnerability.** Follow
  [SECURITY.md](SECURITY.md) instead.
- Please follow our [Code of Conduct](CODE_OF_CONDUCT.md).

## Development setup

ArgosFS targets Linux, uses Rust **1.87 or newer**, and requires `libfuse3`
headers and `pkg-config` to build its FUSE frontend. For a Debian/Ubuntu host:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libfuse3-dev fuse3 smartmontools
rustup toolchain install stable --component rustfmt clippy
```

Install [rustup](https://rustup.rs/) first if it is not already available.
From a fork or checkout:

```sh
cargo build --locked
cargo test --locked --all-targets --all-features -- --test-threads=1
```

Start with [repository layout](docs/repository-layout.md),
[architecture](docs/architecture.md), and [testing](docs/testing.md). The
primary code is under `src/`; integration tests live in `tests/`; host and
mounted-FUSE checks live under `scripts/tests/host/` and `scripts/compat/`;
CapOS/QEMU integration is under `scripts/qemu/`.

## Change and test expectations

Before requesting review, run the checks relevant to your changes. These are
the local equivalents of the core Rust CI checks:

```sh
cargo fmt --package argosfs -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features -- --test-threads=1
RUSTDOCFLAGS='-D warnings' cargo doc --all-features --no-deps
```

For changes to the CLI, on-disk layout, journal, or recovery path, also run
applicable host-side suites, for example:

```sh
scripts/tests/host/cli_features.sh
scripts/tests/host/loop_backend.sh
scripts/tests/host/crash_consistency.sh --unprivileged
```

Check [docs/testing.md](docs/testing.md) for block lifecycle, mounted-FUSE,
permissions, pjdfstest, QEMU, and randomized-model test commands. Some checks
require Linux FUSE access, root privileges, or bootable CapOS images. State
which checks you ran and any environmental skips in the PR; **do not describe a
skipped test as passing**. CI also runs architecture-specific and heavier
integration checks.

When fixing a bug, include a regression test that fails before the fix if
practical. When changing the storage or metadata paths, explicitly assess:

- `fsync`/crash consistency, replay, and recovery from partial writes;
- missing, corrupt, or reordered shards and degraded reads;
- metadata compatibility across versions and upgrades;
- failure behavior (`EIO`, `ENOSPC`, read-only/fail-closed modes);
- safe operation under concurrency and mixed workloads.

The project records its storage guarantees in
[docs/safety-invariants.md](docs/safety-invariants.md). Changes must preserve
those invariants or explain and obtain review for any proposed revision.
For performance work, provide the workload, hardware/kernel, baseline,
measurement method, and before/after results; avoid trading correctness for
throughput without explicit discussion.

**Use disposable image files for destructive tests.** `mkfs`, raw device
operations, repair, and fault injection can overwrite or remove data. Do not
run them on valuable block devices. Privileged test scripts must be reviewed
before pointing them at real devices.

### Erasure coding and dependencies

The native codec under `src/data/ec/` preserves the existing on-disk
parity and reconstruction contract. Changes must pass the legacy oracle,
SIMD differential tests and packaged-source tests. Preserve the third-party
notices alongside adapted code. See [docs/erasure-codec.md](docs/erasure-codec.md).

Keep dependency and lockfile changes intentional and include them in review.

## Pull request workflow

1. Fork the repository (or create a branch if you have write access), then
   branch from the current `main`.
2. Make a focused change, with clear commits and tests near the affected code.
   Split unrelated fixes into separate PRs.
3. Update public documentation when behavior, configuration, on-disk formats,
   or operator safety requirements change.
4. Open a PR targeting `main`, link the relevant issue, explain the behavior
   change and validation, and fill in the PR template.
5. Address review feedback and ensure required CI checks pass. A maintainer
   will decide when the PR can be merged; opening a PR does not guarantee merge.

For a documentation-only PR, explain that runtime tests were not applicable
instead of running privileged integration suites unnecessarily. Avoid
committing generated artifacts, disk images, secrets, private logs, or test
credentials. Redact device identifiers and user data from reproductions.

Contributions are submitted under the repository's
[Apache-2.0 license](LICENSE). Do not submit code you cannot legally
contribute.
