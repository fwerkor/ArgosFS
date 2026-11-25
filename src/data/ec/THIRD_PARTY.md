# Reed–Solomon implementation provenance

The arithmetic, small-erasure recovery algorithm, scalar and SIMD kernels,
and related tests are adapted from reed-solomon-engine 0.2.0:
https://github.com/stephenberry/reed-solomon-engine and
https://crates.io/crates/reed-solomon-engine/0.2.0.
The original distribution was MIT OR Apache-2.0; both original license
notices are retained alongside this file. Original crate archive SHA-256:
`5d902386f55ac0890d3d0f56c932daa0b80b55bcf56631565eed76f0f7f54b6f`.

ArgosFS modifications: internal module paths, removal of standalone/no_std
interfaces, owned codec and presence mask, runtime dispatch and data-path
specialization, new AVX-512/GFNI kernels and cache tiling. The original wire specification is retained in
`docs/erasure-wire-format.md`. New ArgosFS code is Apache-2.0.
