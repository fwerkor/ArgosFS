# Vendored reed-solomon-engine

This directory contains the reed-solomon-engine 0.2.0 source release from
crates.io (MIT OR Apache-2.0). Its source, manifests, documentation,
licenses, and test fixtures are retained unchanged. All ArgosFS codec
adaptation and compatibility checks live outside this vendor directory.

Upstream: https://github.com/stephenberry/reed-solomon-engine
Crate: https://crates.io/crates/reed-solomon-engine/0.2.0

Do not modify SIMD kernels in place without separately auditing CPU feature
dispatch and testing scalar versus every supported vector backend.

Crate archive SHA-256: 5d902386f55ac0890d3d0f56c932daa0b80b55bcf56631565eed76f0f7f54b6f.
