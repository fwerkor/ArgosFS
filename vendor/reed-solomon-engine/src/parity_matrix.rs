//! Compile-time parity matrix construction.

use crate::driver;
use crate::error::{Error, MAX_SHARDS};
use crate::mask::ShardMask;
use crate::matrix;
use crate::parity_matrix_ref::ParityMatrixRef;

/// Largest `data_shards` value [`ParityMatrix::new`] accepts.
///
/// This is now every value [`Codec`](crate::Codec) accepts as well: at
/// least one parity shard is mandatory, so `data_shards` can never exceed
/// `MAX_SHARDS - 1` in the first place.
///
/// It used to be lower, because the const constructor had to size a scratch
/// buffer for a matrix inversion at compile time and so had to pick a bound.
/// The construction no longer inverts anything, which removed the buffer and
/// with it the reason to stop short. The constant stays because callers may be
/// checking against it.
pub const MAX_CONST_DATA_SHARDS: usize = MAX_SHARDS - 1;

/// A parity matrix computed at compile time, and the codec that goes with it.
///
/// This is how `no_std` targets without an allocator get a codec: build the
/// matrix in a `const` or `static`, then call [`encode`](Self::encode) and
/// [`reconstruct`](Self::reconstruct) on it. The matrix lands in `.rodata` and
/// initialization costs nothing at runtime, not even a `OnceLock` check.
///
/// The coefficients are the compiler's, so the shard split is known here with
/// certainty rather than asserted. [`ParityMatrixRef`] is the same codec for
/// coefficients that arrive from somewhere the compiler cannot see, and
/// [`as_ref`](Self::as_ref) crosses over.
///
/// `CAP` is the capacity of the inline coefficient array and must be at least
/// `data_shards * parity_shards`.
///
/// ```
/// use reed_solomon_engine::ParityMatrix;
///
/// static MATRIX: ParityMatrix<8> = ParityMatrix::new(4, 2);
///
/// let data: [&[u8]; 4] = [b"aaaa", b"bbbb", b"cccc", b"dddd"];
/// let (mut p0, mut p1) = ([0u8; 4], [0u8; 4]);
/// let mut parity: [&mut [u8]; 2] = [&mut p0, &mut p1];
///
/// MATRIX.encode(&data, &mut parity).unwrap();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParityMatrix<const CAP: usize> {
    data_shards: u16,
    parity_shards: u16,
    coefficients: [u8; CAP],
}

impl<const CAP: usize> ParityMatrix<CAP> {
    /// Builds the systematic parity matrix for `data_shards` and
    /// `parity_shards`.
    ///
    /// Intended for `const` and `static` contexts. Calling it at runtime works
    /// and is cheap, but prefer [`Codec::new`](crate::Codec::new) there:
    /// it reports an unusable shard split as an [`Error`] rather than a panic.
    ///
    /// # Compile time
    ///
    /// Building the matrix costs `O(data_shards^2)` field operations, and the
    /// compiler's const evaluator counts every one of them against a step
    /// budget guarded by the deny-by-default `long_running_const_eval` lint.
    /// The largest split this crate can express, 255 data shards, stays well
    /// inside that budget: about 0.6 seconds of compile time on an M-series
    /// laptop, with no lint waiver needed at any size.
    ///
    /// This was not always true. The cubic construction that preceded the
    /// closed form exceeded the budget past roughly 83 data shards, and 128
    /// data shards took 36 seconds and an `#![allow(long_running_const_eval)]`
    /// in the calling crate. The coefficients are unchanged; only the cost of
    /// producing them is.
    ///
    /// # Panics
    ///
    /// Panics, which is a compile error in a `const` context, if:
    ///
    /// - `data_shards` or `parity_shards` is zero,
    /// - `data_shards + parity_shards` exceeds
    ///   [`MAX_SHARDS`](crate::MAX_SHARDS),
    /// - `data_shards` exceeds [`MAX_CONST_DATA_SHARDS`], or
    /// - `CAP` is smaller than `data_shards * parity_shards`.
    pub const fn new(data_shards: usize, parity_shards: usize) -> Self {
        assert!(data_shards >= 1, "data_shards must be at least 1");
        assert!(parity_shards >= 1, "parity_shards must be at least 1");
        assert!(
            data_shards + parity_shards <= MAX_SHARDS,
            "data_shards + parity_shards must not exceed 256"
        );
        // Implied by the two asserts above while `MAX_CONST_DATA_SHARDS` is
        // `MAX_SHARDS - 1`. Kept because that constant is public, so this is
        // the check that keeps its documented promise honest if it ever moves.
        assert!(
            data_shards <= MAX_CONST_DATA_SHARDS,
            "data_shards must not exceed MAX_CONST_DATA_SHARDS"
        );
        assert!(
            CAP >= data_shards * parity_shards,
            "CAP must be at least data_shards * parity_shards"
        );

        let mut coefficients = [0u8; CAP];
        assert!(
            matrix::build_parity_matrix(data_shards, parity_shards, &mut coefficients),
            "the construction refuses only splits the asserts above have already rejected"
        );

        Self {
            data_shards: data_shards as u16,
            parity_shards: parity_shards as u16,
            coefficients,
        }
    }

    /// The `parity_shards * data_shards` row-major coefficient matrix.
    #[inline]
    pub fn coefficients(&self) -> &[u8] {
        &self.coefficients[..self.data_shards as usize * self.parity_shards as usize]
    }

    /// Number of data shards this matrix was built for.
    #[inline]
    pub const fn data_shards(&self) -> usize {
        self.data_shards as usize
    }

    /// Number of parity shards this matrix was built for.
    #[inline]
    pub const fn parity_shards(&self) -> usize {
        self.parity_shards as usize
    }

    /// Total number of shards, data plus parity.
    #[inline]
    pub const fn total_shards(&self) -> usize {
        self.data_shards as usize + self.parity_shards as usize
    }

    /// Bytes of workspace [`reconstruct`](Self::reconstruct) needs.
    ///
    /// `const`, so the buffer can be sized at compile time too:
    ///
    /// ```
    /// # use reed_solomon_engine::ParityMatrix;
    /// static MATRIX: ParityMatrix<8> = ParityMatrix::new(4, 2);
    /// let mut workspace = [0u8; MATRIX.workspace_len()];
    /// # let _ = &mut workspace;
    /// ```
    #[inline]
    pub const fn workspace_len(&self) -> usize {
        driver::workspace_len(self.data_shards as usize, self.parity_shards as usize)
    }

    /// Writes the parity shards for `data` into `parity`.
    ///
    /// All shards must have the same non-zero length. The previous contents of
    /// `parity` are ignored.
    ///
    /// Never allocates.
    pub fn encode<D, P>(&self, data: &[D], parity: &mut [P]) -> Result<(), Error>
    where
        D: AsRef<[u8]>,
        P: AsRef<[u8]> + AsMut<[u8]>,
    {
        self.as_ref().encode(data, parity)
    }

    /// Writes the parity shards for a single slice holding the data shards
    /// followed by the parity shards.
    ///
    /// Never allocates.
    pub fn encode_all<S: AsRef<[u8]> + AsMut<[u8]>>(&self, shards: &mut [S]) -> Result<(), Error> {
        self.as_ref().encode_all(shards)
    }

    /// Reconstructs every missing shard in place.
    ///
    /// `shards` holds the data shards followed by the parity shards. Shards
    /// marked missing in `present` are treated as scratch: their contents are
    /// ignored and overwritten. `workspace` must be at least
    /// [`workspace_len`](Self::workspace_len) bytes.
    ///
    /// Never allocates.
    pub fn reconstruct<S: AsRef<[u8]> + AsMut<[u8]>>(
        &self,
        shards: &mut [S],
        present: &ShardMask,
        workspace: &mut [u8],
    ) -> Result<(), Error> {
        self.as_ref().reconstruct(shards, present, workspace)
    }

    /// Reconstructs only the missing data shards, leaving missing parity
    /// shards holding whatever they held on entry.
    ///
    /// Never allocates.
    pub fn reconstruct_data<S: AsRef<[u8]> + AsMut<[u8]>>(
        &self,
        shards: &mut [S],
        present: &ShardMask,
        workspace: &mut [u8],
    ) -> Result<(), Error> {
        self.as_ref().reconstruct_data(shards, present, workspace)
    }

    /// Borrows this matrix as a [`ParityMatrixRef`].
    ///
    /// The two carry the same information; this one owns its coefficients and
    /// is `CAP`-generic, that one borrows them and is not. Crossing over lets a
    /// caller hold a compile-time matrix and a runtime-loaded one in the same
    /// variable, and is how the four operations above reach the codec.
    #[inline]
    pub fn as_ref(&self) -> ParityMatrixRef<'_> {
        ParityMatrixRef::new_unchecked(self.data_shards, self.parity_shards, self.coefficients())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn const_matrix_reports_its_shape() {
        const M: ParityMatrix<8> = ParityMatrix::new(4, 2);
        assert_eq!(M.data_shards(), 4);
        assert_eq!(M.parity_shards(), 2);
        assert_eq!(M.total_shards(), 6);
        assert_eq!(M.coefficients().len(), 8);
    }

    #[test]
    fn oversized_capacity_is_trimmed() {
        const M: ParityMatrix<64> = ParityMatrix::new(4, 2);
        assert_eq!(M.coefficients().len(), 8);
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn matches_the_runtime_codec() {
        const M: ParityMatrix<8> = ParityMatrix::new(4, 2);
        let codec = crate::Codec::new(4, 2).unwrap();
        assert_eq!(M.coefficients(), codec.parity_matrix());
    }

    #[test]
    fn a_transposed_split_is_rejected() {
        // The whole reason these methods exist. A bare coefficient slice cannot
        // tell a 4+2 matrix from a 2+4 one, because both hold 8 bytes; a
        // ParityMatrix knows which it is, and had no chance to be told wrong.
        const M: ParityMatrix<8> = ParityMatrix::new(4, 2);

        let data: [&[u8]; 2] = [b"aaaa", b"bbbb"];
        let mut buffers = [[0u8; 4]; 4];
        let [p0, p1, p2, p3] = &mut buffers;
        let mut parity: [&mut [u8]; 4] = [p0, p1, p2, p3];

        assert_eq!(
            M.encode(&data, &mut parity),
            Err(Error::ShardCountMismatch {
                expected: 4,
                actual: 2
            })
        );
    }

    #[test]
    fn borrowing_preserves_the_split_and_the_coefficients() {
        const M: ParityMatrix<64> = ParityMatrix::new(4, 2);
        let borrowed = M.as_ref();
        assert_eq!(borrowed.data_shards(), M.data_shards());
        assert_eq!(borrowed.parity_shards(), M.parity_shards());
        // Trimmed to the live prefix, not the whole `CAP` array.
        assert_eq!(borrowed.coefficients(), M.coefficients());
    }

    #[test]
    fn round_trip_through_the_const_path() {
        const M: ParityMatrix<8> = ParityMatrix::new(4, 2);
        let mut workspace = [0u8; M.workspace_len()];

        let mut buffers = [*b"west", *b"east", *b"nrth", *b"suth", [0; 4], [0; 4]];
        let [a, b, c, d, e, f] = &mut buffers;
        let mut shards: [&mut [u8]; 6] = [a, b, c, d, e, f];
        M.encode_all(&mut shards).unwrap();

        let mut present = ShardMask::all_present(6);
        present.remove(1);
        present.remove(4);
        shards[1].fill(0xA5);
        shards[4].fill(0xA5);

        M.reconstruct(&mut shards, &present, &mut workspace)
            .unwrap();
        assert_eq!(shards[1], b"east");
        assert_ne!(shards[4], &[0xA5u8; 4]);
    }
}
