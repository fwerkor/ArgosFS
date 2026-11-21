# Vendored reed-solomon-engine 0.2.0

Source: https://crates.io/crates/reed-solomon-engine/0.2.0
Upstream: https://github.com/stephenberry/reed-solomon-engine
License: MIT OR Apache-2.0, with notices in LICENSE-MIT and LICENSE-APACHE.
Published crate archive SHA-256:
5d902386f55ac0890d3d0f56c932daa0b80b55bcf56631565eed76f0f7f54b6f

All files under src/ are byte-for-byte identical to the published 0.2.0
crate archive. This pinned snapshot includes only the production source,
wire-format/design documentation, README, licenses, and a minimal manifest.
Unused upstream tests, benchmarks and experimental examples were omitted.
ArgosFS owns compatibility tests against the former erase 4.0.0 codec.

The local Cargo.toml is intentionally simplified and publish=false to prevent
accidental independent publication. Upstream source should be updated by
re-importing a validated release archive, never by silently changing SIMD.
Check the full vendor SHA-256 manifest in CODE_HASHES.sha256.

Cargo dependencies declared with both a path and version use the local source
in a repository checkout, but a published ArgosFS crate resolves the registry
release of reed-solomon-engine 0.2.0. The source byte identity protects that
behavior; verify with the package smoke test before releases.
