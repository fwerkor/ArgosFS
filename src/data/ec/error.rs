//! Error type.

use core::fmt;

/// The maximum number of shards a single codec can address.
///
/// GF(2^8) has 256 elements, and the generator matrix needs one distinct
/// element per shard.
pub const MAX_SHARDS: usize = 256;

/// Everything that can go wrong.
///
/// The variants are `Copy` and carry no allocation, so this type is usable in
/// the tightest `no_std` context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Error {
    /// `data_shards` was zero.
    NoDataShards,
    /// `parity_shards` was zero.
    NoParityShards,
    /// `data_shards + parity_shards` exceeded [`MAX_SHARDS`].
    TooManyShards {
        /// The total that was requested.
        total: usize,
    },
    /// The caller passed a different number of shards than the codec was
    /// constructed for.
    ShardCountMismatch {
        /// How many shards the codec expected.
        expected: usize,
        /// How many the caller supplied.
        actual: usize,
    },
    /// Shards were not all the same length.
    ShardLengthMismatch,
    /// A shard had zero length.
    EmptyShard,
    /// Fewer than `data_shards` shards were marked present, so reconstruction
    /// is information-theoretically impossible.
    TooFewShards {
        /// How many shards were marked present.
        present: usize,
        /// How many are needed.
        required: usize,
    },
    /// A caller-supplied coefficient slice did not have length
    /// `parity_shards * data_shards`.
    MatrixSizeMismatch {
        /// The length the codec expected.
        expected: usize,
        /// The length that was supplied.
        actual: usize,
    },
    /// A matrix that the code's MDS property guarantees to be invertible was
    /// found to be singular.
    ///
    /// Reaching this variant means the generator matrix is not the one this
    /// crate constructs; it cannot be triggered by any input to a codec built
    /// through [`Codec::new`](crate::data::ec::Codec::new) or
    /// [`Codec::new`](crate::data::ec::Codec::new).
    SingularMatrix,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::NoDataShards => f.write_str("data_shards must be at least 1"),
            Error::NoParityShards => f.write_str("parity_shards must be at least 1"),
            Error::TooManyShards { total } => {
                write!(
                    f,
                    "{total} total shards exceeds the maximum of {MAX_SHARDS}"
                )
            }
            Error::ShardCountMismatch { expected, actual } => {
                write!(f, "expected {expected} shards, got {actual}")
            }
            Error::ShardLengthMismatch => f.write_str("all shards must have the same length"),
            Error::EmptyShard => f.write_str("shards must not be empty"),
            Error::TooFewShards { present, required } => {
                write!(f, "{present} shards present, at least {required} required")
            }
            Error::MatrixSizeMismatch { expected, actual } => {
                write!(f, "expected {expected} matrix coefficients, got {actual}")
            }
            Error::SingularMatrix => f.write_str("matrix is singular"),
        }
    }
}

impl core::error::Error for Error {}
