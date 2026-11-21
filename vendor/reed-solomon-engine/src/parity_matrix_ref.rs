//! Borrowed parity matrix carrying the shard split it was built for.

use crate::driver;
use crate::error::Error;
use crate::kernel::Kernel;
use crate::mask::ShardMask;

/// A parity matrix that lives somewhere else, together with the data/parity
/// split it was built for.
///
/// This is the allocation-free codec for coefficients this crate did not
/// build: a matrix read from a file, from flash, or from a linker section, or
/// one whose split is only known once the program is running.
/// [`ParityMatrix`](crate::ParityMatrix) is the same codec for coefficients
/// built at compile time, and [`Codec`](crate::Codec) for coefficients
/// built at runtime with an allocator; both borrow as this type, so all three
/// paths run the same checked code.
///
/// Nothing here allocates and nothing here can panic.
///
/// # Why the split is part of the type
///
/// A bare `&[u8]` of coefficients cannot say what shape it is: a matrix built
/// for `k + m` holds exactly as many bytes as one built for `m + k`. Carrying
/// the split means it is stated once, next to the bytes it describes, and every
/// operation is checked against it. The alternative, restating it at every
/// call, is a silent-corruption hazard rather than an error: a decoder handed
/// the transposed split returns `Ok` and plausible bytes for every erasure
/// pattern, so nothing detects it until a receiver actually loses a shard.
///
/// ```
/// use reed_solomon_engine::ParityMatrixRef;
///
/// // Coefficients from anywhere: a file, flash, a linker section.
/// let coefficients: &[u8] = &[27, 28, 18, 20, 28, 27, 20, 18];
/// let matrix = ParityMatrixRef::new(4, 2, coefficients)?;
///
/// let data: [&[u8]; 4] = [b"aaaa", b"bbbb", b"cccc", b"dddd"];
/// let (mut p0, mut p1) = ([0u8; 4], [0u8; 4]);
/// let mut parity: [&mut [u8]; 2] = [&mut p0, &mut p1];
///
/// matrix.encode(&data, &mut parity)?;
/// # Ok::<(), reed_solomon_engine::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParityMatrixRef<'a> {
    data_shards: u16,
    parity_shards: u16,
    kernel: Option<Kernel>,
    coefficients: &'a [u8],
}

impl<'a> ParityMatrixRef<'a> {
    /// Borrows `coefficients` as the parity matrix for a `data_shards +
    /// parity_shards` code.
    ///
    /// `coefficients` is row-major with `data_shards` columns and
    /// `parity_shards` rows, which is the layout
    /// [`ParityMatrix::coefficients`](crate::ParityMatrix::coefficients) and
    /// [`Codec::parity_matrix`](crate::Codec::parity_matrix) produce and
    /// the one `docs/WIRE_FORMAT.md` specifies.
    ///
    /// Fails if either count is zero, if their sum exceeds
    /// [`MAX_SHARDS`](crate::MAX_SHARDS), or if `coefficients` is not exactly
    /// `data_shards * parity_shards` bytes long.
    ///
    /// # What this does not check
    ///
    /// The split is the caller's assertion about bytes only the caller knows
    /// the provenance of, and the only structural evidence available is the
    /// coefficient count. `ParityMatrixRef::new(2, 4, coefficients)` on the
    /// coefficients of a 4+2 matrix therefore succeeds: both shapes hold eight
    /// bytes, and nothing in those bytes says which. Encoding through the
    /// result produces deterministic, plausible, undecodable parity.
    ///
    /// What carrying the split buys is that the assertion is made exactly once,
    /// at the point where the bytes are obtained and their shape is actually
    /// known, instead of being restated at every encode and decode call where
    /// it can drift. Get it right here and no later call site can get it wrong.
    ///
    /// # Const
    ///
    /// `const`, so a matrix in `.rodata` can be wrapped in a `const` too:
    ///
    /// ```
    /// # use reed_solomon_engine::ParityMatrixRef;
    /// static COEFFICIENTS: [u8; 8] = [27, 28, 18, 20, 28, 27, 20, 18];
    /// const MATRIX: ParityMatrixRef<'static> =
    ///     match ParityMatrixRef::new(4, 2, &COEFFICIENTS) {
    ///         Ok(matrix) => matrix,
    ///         Err(_) => panic!("4 * 2 coefficients"),
    ///     };
    /// # assert_eq!(MATRIX.data_shards(), 4);
    /// ```
    pub const fn new(
        data_shards: usize,
        parity_shards: usize,
        coefficients: &'a [u8],
    ) -> Result<Self, Error> {
        // The same two checks every operation runs, called rather than
        // restated: a second copy that had to agree with the first would be the
        // very drift this type exists to remove. `?` is not available on
        // `Result` in a `const fn`, so they are matched instead of propagated.
        if let Err(error) = driver::check_counts(data_shards, parity_shards) {
            return Err(error);
        }
        if let Err(error) = driver::check_matrix(coefficients, data_shards, parity_shards) {
            return Err(error);
        }
        Ok(Self {
            data_shards: data_shards as u16,
            parity_shards: parity_shards as u16,
            kernel: None,
            coefficients,
        })
    }

    /// Builds a view whose shape the caller has already established.
    ///
    /// [`ParityMatrix`](crate::ParityMatrix), [`Codec`](crate::Codec) and
    /// [`Codec`](crate::Codec) hold the same invariants by construction, so
    /// routing them through [`new`](Self::new) would put an unreachable error
    /// arm in a crate whose whole point is that these paths cannot fail.
    #[inline]
    pub(crate) const fn new_unchecked(
        data_shards: u16,
        parity_shards: u16,
        coefficients: &'a [u8],
    ) -> Self {
        Self {
            data_shards,
            parity_shards,
            kernel: None,
            coefficients,
        }
    }

    /// Returns the same matrix with a caller-chosen inner kernel.
    ///
    /// [`new`](Self::new) leaves the kernel unresolved and every operation then
    /// calls [`Kernel::detect`]; this resolves it once instead. Output bytes
    /// are identical either way.
    #[inline]
    pub const fn with_kernel(mut self, kernel: Kernel) -> Self {
        self.kernel = Some(kernel);
        self
    }

    /// The borrowed row-major coefficients.
    #[inline]
    pub const fn coefficients(&self) -> &'a [u8] {
        self.coefficients
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
    #[inline]
    pub const fn workspace_len(&self) -> usize {
        driver::workspace_len(self.data_shards as usize, self.parity_shards as usize)
    }

    /// Writes the parity shards for `data` into `parity`.
    ///
    /// `data` must hold [`data_shards`](Self::data_shards) shards and `parity`
    /// [`parity_shards`](Self::parity_shards) of them, all the same non-zero
    /// length. The previous contents of `parity` are ignored.
    ///
    /// Never allocates.
    pub fn encode<D, P>(&self, data: &[D], parity: &mut [P]) -> Result<(), Error>
    where
        D: AsRef<[u8]>,
        P: AsRef<[u8]> + AsMut<[u8]>,
    {
        driver::encode_with_kernel(
            self.kernel(),
            self.coefficients,
            self.data_shards(),
            self.parity_shards(),
            data,
            parity,
        )
    }

    /// Writes the parity shards for a single slice holding the data shards
    /// followed by the parity shards.
    ///
    /// This is the layout [`reconstruct`](Self::reconstruct) expects, so a
    /// sender that keeps one shard buffer can use it for both directions.
    ///
    /// Never allocates.
    pub fn encode_all<S: AsRef<[u8]> + AsMut<[u8]>>(&self, shards: &mut [S]) -> Result<(), Error> {
        driver::encode_all_with_kernel(
            self.kernel(),
            self.coefficients,
            self.data_shards(),
            self.parity_shards(),
            shards,
        )
    }

    /// Reconstructs every missing shard in place.
    ///
    /// `shards` holds the data shards followed by the parity shards. Shards
    /// marked missing in `present` are treated as scratch: their contents are
    /// ignored and overwritten. All shards must have the same non-zero length,
    /// including the missing ones. `workspace` must be at least
    /// [`workspace_len`](Self::workspace_len) bytes and its contents on entry
    /// are ignored.
    ///
    /// Fails with [`Error::TooFewShards`] if fewer than
    /// [`data_shards`](Self::data_shards) shards are present.
    ///
    /// Never allocates.
    pub fn reconstruct<S: AsRef<[u8]> + AsMut<[u8]>>(
        &self,
        shards: &mut [S],
        present: &ShardMask,
        workspace: &mut [u8],
    ) -> Result<(), Error> {
        driver::reconstruct_impl(
            self.kernel(),
            self.coefficients,
            self.data_shards(),
            self.parity_shards(),
            shards,
            present,
            workspace,
            true,
        )
    }

    /// Reconstructs only the missing data shards, leaving missing parity shards
    /// holding whatever they held on entry.
    ///
    /// Prefer this when the parity shards are about to be discarded: it saves
    /// `data_shards` shard operations per missing parity shard.
    ///
    /// Never allocates.
    pub fn reconstruct_data<S: AsRef<[u8]> + AsMut<[u8]>>(
        &self,
        shards: &mut [S],
        present: &ShardMask,
        workspace: &mut [u8],
    ) -> Result<(), Error> {
        driver::reconstruct_impl(
            self.kernel(),
            self.coefficients,
            self.data_shards(),
            self.parity_shards(),
            shards,
            present,
            workspace,
            false,
        )
    }

    /// `new` cannot call [`Kernel::detect`] because it is a `const fn`, so a
    /// view with no pinned kernel detects once per operation.
    #[inline]
    fn kernel(&self) -> Kernel {
        self.kernel.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parity_matrix::ParityMatrix;

    const COEFFICIENTS_4_2: [u8; 8] = [27, 28, 18, 20, 28, 27, 20, 18];

    #[test]
    fn a_view_reports_the_split_it_was_built_for() {
        let matrix = ParityMatrixRef::new(4, 2, &COEFFICIENTS_4_2).unwrap();
        assert_eq!(matrix.data_shards(), 4);
        assert_eq!(matrix.parity_shards(), 2);
        assert_eq!(matrix.total_shards(), 6);
        assert_eq!(matrix.workspace_len(), driver::workspace_len(4, 2));
        assert_eq!(matrix.coefficients(), &COEFFICIENTS_4_2);
    }

    #[test]
    fn the_pinned_kernel_survives_the_builder() {
        let matrix = ParityMatrixRef::new(4, 2, &COEFFICIENTS_4_2)
            .unwrap()
            .with_kernel(Kernel::SCALAR);
        assert_eq!(matrix.kernel(), Kernel::SCALAR);
    }

    #[test]
    fn a_zero_count_or_an_oversized_total_is_refused() {
        assert_eq!(ParityMatrixRef::new(0, 2, &[]), Err(Error::NoDataShards));
        assert_eq!(ParityMatrixRef::new(4, 0, &[]), Err(Error::NoParityShards));
        assert_eq!(
            ParityMatrixRef::new(200, 57, &[]),
            Err(Error::TooManyShards { total: 257 })
        );
        // A sum that would wrap must be reported as oversized, not folded back
        // into the legal range.
        assert_eq!(
            ParityMatrixRef::new(usize::MAX, 8, &[]),
            Err(Error::TooManyShards { total: usize::MAX })
        );
    }

    #[test]
    fn a_coefficient_count_that_does_not_match_the_split_is_refused() {
        assert_eq!(
            ParityMatrixRef::new(4, 2, &COEFFICIENTS_4_2[..7]),
            Err(Error::MatrixSizeMismatch {
                expected: 8,
                actual: 7
            })
        );
        assert_eq!(
            ParityMatrixRef::new(4, 3, &COEFFICIENTS_4_2),
            Err(Error::MatrixSizeMismatch {
                expected: 12,
                actual: 8
            })
        );
    }

    #[test]
    fn a_transposed_split_is_rejected_wherever_the_shard_counts_can_see_it() {
        // 4+2 and 2+4 hold the same eight coefficients, so the constructor has
        // no structural evidence to reject the transpose with and accepts it.
        let transposed = ParityMatrixRef::new(2, 4, &COEFFICIENTS_4_2).unwrap();
        assert_eq!(transposed.data_shards(), 2);

        // The split form does see it, because `data` and `parity` are counted
        // separately against the claim rather than only in total.
        let data: [&[u8]; 4] = [b"aaaa", b"bbbb", b"cccc", b"dddd"];
        let mut parity_buffers = [[0u8; 4]; 2];
        let mut parity: [&mut [u8]; 2] = parity_buffers.each_mut().map(|b| b.as_mut_slice());
        assert_eq!(
            transposed.encode(&data, &mut parity),
            Err(Error::ShardCountMismatch {
                expected: 2,
                actual: 4
            })
        );
    }

    #[test]
    fn a_transposed_split_still_corrupts_the_combined_form_silently() {
        // The residual hazard, asserted rather than only documented. Both
        // splits total six shards, so `encode_all` has nothing to compare and
        // succeeds; the parity it writes is for a code nothing can decode.
        // This is the reason the constructor's claim has to be right: it is the
        // only place the truth is available.
        let honest = ParityMatrixRef::new(4, 2, &COEFFICIENTS_4_2).unwrap();
        let transposed = ParityMatrixRef::new(2, 4, &COEFFICIENTS_4_2).unwrap();
        assert_eq!(honest.total_shards(), transposed.total_shards());

        let payload = [*b"west", *b"east", *b"nrth", *b"suth", [0; 4], [0; 4]];

        let mut from_honest = payload;
        let [a, b, c, d, e, f] = &mut from_honest;
        honest.encode_all(&mut [a, b, c, d, e, f]).unwrap();

        let mut from_transposed = payload;
        let [a, b, c, d, e, f] = &mut from_transposed;
        transposed.encode_all(&mut [a, b, c, d, e, f]).unwrap();

        assert_ne!(from_honest, from_transposed);
    }

    #[test]
    fn a_shard_count_that_does_not_match_the_split_is_refused() {
        let matrix = ParityMatrixRef::new(4, 2, &COEFFICIENTS_4_2).unwrap();

        let data: [&[u8]; 3] = [b"aaaa", b"bbbb", b"cccc"];
        let mut parity_buffers = [[0u8; 4]; 2];
        let mut parity: [&mut [u8]; 2] = parity_buffers.each_mut().map(|b| b.as_mut_slice());
        assert_eq!(
            matrix.encode(&data, &mut parity),
            Err(Error::ShardCountMismatch {
                expected: 4,
                actual: 3
            })
        );

        let mut buffers = [[0u8; 4]; 5];
        let mut shards: [&mut [u8]; 5] = buffers.each_mut().map(|b| b.as_mut_slice());
        assert_eq!(
            matrix.encode_all(&mut shards),
            Err(Error::ShardCountMismatch {
                expected: 6,
                actual: 5
            })
        );
    }

    #[test]
    fn round_trip_through_a_borrowed_matrix() {
        let matrix = ParityMatrixRef::new(4, 2, &COEFFICIENTS_4_2).unwrap();
        let mut workspace = [0u8; 12];

        let mut buffers = [*b"west", *b"east", *b"nrth", *b"suth", [0; 4], [0; 4]];
        let [a, b, c, d, e, f] = &mut buffers;
        let mut shards: [&mut [u8]; 6] = [a, b, c, d, e, f];
        matrix.encode_all(&mut shards).unwrap();

        let mut present = ShardMask::all_present(6);
        present.remove(1);
        present.remove(4);
        shards[1].fill(0xA5);
        shards[4].fill(0xA5);

        matrix
            .reconstruct(&mut shards, &present, &mut workspace)
            .unwrap();
        assert_eq!(shards[1], b"east");
        assert_ne!(shards[4], &[0xA5u8; 4]);
    }

    #[test]
    fn a_borrowed_const_matrix_agrees_with_the_owned_one() {
        const OWNED: ParityMatrix<8> = ParityMatrix::new(4, 2);
        assert_eq!(OWNED.coefficients(), &COEFFICIENTS_4_2);

        let borrowed = ParityMatrixRef::new(4, 2, &COEFFICIENTS_4_2).unwrap();
        let data: [&[u8]; 4] = [b"aaaa", b"bbbb", b"cccc", b"dddd"];

        let mut from_owned = [[0u8; 4]; 2];
        let mut from_borrowed = [[0u8; 4]; 2];
        OWNED
            .encode(&data, &mut from_owned.each_mut().map(|b| b.as_mut_slice()))
            .unwrap();
        borrowed
            .encode(
                &data,
                &mut from_borrowed.each_mut().map(|b| b.as_mut_slice()),
            )
            .unwrap();
        assert_eq!(from_owned, from_borrowed);
    }
}
