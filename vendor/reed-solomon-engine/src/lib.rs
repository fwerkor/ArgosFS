//! Reed-Solomon erasure coding over GF(2^8), with no dependencies.
//!
//! This crate recovers shards that are *known to be missing*. It does not
//! detect corruption: verifying integrity is the caller's job, typically with a
//! checksum carried alongside each shard.
//!
//! # What makes it different
//!
//! - **Zero runtime dependencies.** `[dependencies]` is empty and is meant to
//!   stay that way.
//! - **`no_std`, and `no_std` without an allocator.** The encode and decode
//!   primitives operate on caller-provided buffers.
//!   [`ParityMatrix::new`] builds the generator matrix at compile time.
//! - **Zero initialization cost.** Every table, including the 8 KiB of SIMD
//!   shuffle tables, is `const fn` evaluated. No lazy-init check, no lock,
//!   nothing to warm up.
//! - **SIMD kernels selected at runtime.** See [`Kernel`]. The scalar
//!   reference stays as the specification and every other kernel is tested
//!   byte-identical to it.
//! - **Decode cost scales with the number of erasures.** Recovering two lost
//!   shards out of eighty solves a 2x2 system, not an 80x80 one.
//!
//! # Wire format
//!
//! The generator matrix is the systematic Vandermonde construction over
//! GF(2^8) with reduction polynomial `0x11D`, which makes the parity bytes
//! byte-identical to the standard GF(2^8) matrix format that the widely
//! deployed implementations of this code use. It is specified in
//! `docs/WIRE_FORMAT.md` and pinned
//! by the known-answer vectors in `tests/`. Changing it is a semver-major
//! change.
//!
//! # Getting started
//!
//! ```
//! use reed_solomon_engine::{Codec, ShardMask};
//!
//! let codec = Codec::new(4, 2)?;
//! let mut workspace = codec.workspace();
//!
//! // Data shards first, then space for the parity shards. Any container of
//! // byte slices will do; these happen to be owned buffers.
//! let mut shards: Vec<Vec<u8>> = vec![
//!     b"alpha".to_vec(),
//!     b"bravo".to_vec(),
//!     b"charl".to_vec(),
//!     b"delta".to_vec(),
//!     vec![0; 5],
//!     vec![0; 5],
//! ];
//!
//! codec.encode_all(&mut shards)?;
//!
//! // Any two shards may go missing.
//! let mut present = ShardMask::all_present(6);
//! present.remove(1);
//! present.remove(5);
//!
//! codec.reconstruct(&mut shards, &present, &mut workspace)?;
//! assert_eq!(shards[1], b"bravo");
//! # Ok::<(), reed_solomon_engine::Error>(())
//! ```
//!
//! One [`Codec`] does both directions and every method takes `&self`, so it can
//! be shared, including across threads, with one workspace per decoding thread.
//!
//! # Without an allocator
//!
//! ```
//! use reed_solomon_engine::ParityMatrix;
//!
//! // Built at compile time, lives in .rodata, costs nothing to initialize.
//! static MATRIX: ParityMatrix<8> = ParityMatrix::new(4, 2);
//!
//! let data: [&[u8]; 4] = [b"alpha", b"bravo", b"charl", b"delta"];
//! let (mut p0, mut p1) = ([0u8; 5], [0u8; 5]);
//! let mut parity: [&mut [u8]; 2] = [&mut p0, &mut p1];
//!
//! MATRIX.encode(&data, &mut parity)?;
//! # Ok::<(), reed_solomon_engine::Error>(())
//! ```
//!
//! Decoding needs a workspace, which can also be sized at compile time:
//!
//! ```
//! # use reed_solomon_engine::{ParityMatrix, ShardMask};
//! static MATRIX: ParityMatrix<8> = ParityMatrix::new(4, 2);
//! let mut workspace = [0u8; MATRIX.workspace_len()];
//!
//! # let mut buffers = [*b"alpha", *b"bravo", *b"charl", *b"delta", [0; 5], [0; 5]];
//! # let mut shards: [&mut [u8]; 6] = { let [a,b,c,d,e,f] = &mut buffers; [a,b,c,d,e,f] };
//! # MATRIX.encode_all(&mut shards)?;
//! # let mut present = ShardMask::all_present(6);
//! # present.remove(2);
//! MATRIX.reconstruct(&mut shards, &present, &mut workspace)?;
//! # assert_eq!(shards[2], b"charl");
//! # Ok::<(), reed_solomon_engine::Error>(())
//! ```
//!
//! # Feature flags
//!
//! | feature | default | effect |
//! |---|---|---|
//! | `std` | yes | implies `alloc`; enables runtime CPU feature detection |
//! | `alloc` | via `std` | enables [`Codec`] |
//!
//! With no features the crate is `no_std` and allocation-free; use
//! [`ParityMatrix`] for a matrix the compiler builds, or [`ParityMatrixRef`]
//! for coefficients that arrive from somewhere else.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
// `unsafe` is permitted only inside the SIMD kernel modules, which opt in with
// an inner `allow`, plus the three dispatch methods on `kernel::Kernel` that
// enter them. Every block names the feature check that makes it sound. Nowhere
// else.
#![deny(unsafe_code)]
#![deny(missing_docs)]
#![warn(clippy::all)]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(test)]
extern crate std;

mod driver;
mod error;
mod kernel;
mod mask;
mod matrix;
mod parity_matrix;
mod parity_matrix_ref;
mod shards;

pub mod gf;

pub use driver::workspace_len;
pub use error::{Error, MAX_SHARDS};
pub use kernel::Kernel;
pub use mask::ShardMask;
pub use parity_matrix::{MAX_CONST_DATA_SHARDS, ParityMatrix};
pub use parity_matrix_ref::ParityMatrixRef;

#[cfg(feature = "alloc")]
mod owned;

#[cfg(feature = "alloc")]
#[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
pub use owned::Codec;
