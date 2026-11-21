//! Owned-matrix codec, for targets with an allocator.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::driver;
use crate::error::Error;
use crate::kernel::Kernel;
use crate::mask::ShardMask;
use crate::matrix;
use crate::parity_matrix_ref::ParityMatrixRef;

/// A Reed-Solomon codec for a fixed `(data_shards, parity_shards)` split, whose
/// generator matrix it builds and owns.
///
/// Building a `Codec` computes the generator matrix, which is the only
/// allocation the type ever performs. Every operation is allocation-free, so a
/// `Codec` is meant to be constructed once and reused for the lifetime of a
/// stream.
///
/// ```
/// use reed_solomon_engine::{Codec, ShardMask};
///
/// let codec = Codec::new(4, 2)?;
/// let mut workspace = codec.workspace();
///
/// // Any container of byte slices works; these are owned buffers.
/// let mut shards: Vec<Vec<u8>> = vec![
///     b"alpha".to_vec(),
///     b"bravo".to_vec(),
///     b"charl".to_vec(),
///     b"delta".to_vec(),
///     vec![0; 5],
///     vec![0; 5],
/// ];
/// codec.encode_all(&mut shards)?;
///
/// // Lose any two.
/// let mut present = ShardMask::all_present(6);
/// present.remove(1);
/// present.remove(5);
///
/// codec.reconstruct(&mut shards, &present, &mut workspace)?;
/// assert_eq!(shards[1], b"bravo");
/// # Ok::<(), reed_solomon_engine::Error>(())
/// ```
///
/// # Why one type for both directions
///
/// This replaces the separate `Encoder` and `Decoder` of 0.1.0. Those two held
/// the same generator matrix and differed in exactly one field: the decode
/// scratch buffer. Splitting a codec by direction rather than by the data it
/// owns meant a caller that both sent and received built the same matrix twice,
/// and it left the `alloc` layer as the only one in the crate that could not do
/// both directions from one value, unlike [`ParityMatrix`](crate::ParityMatrix)
/// and [`ParityMatrixRef`](crate::ParityMatrixRef), which always could.
///
/// # Why the workspace is the caller's
///
/// A generator matrix is immutable, shareable and worth keeping; decode scratch
/// is neither. Owning both in one value forces the second one's mutability onto
/// the first, which is what made 0.1.0's `Decoder::reconstruct` take `&mut
/// self`: logically it only ever *read* the matrix. The cost of that was real.
/// A codec behind a shared reference could not decode at all, so sharing one
/// across threads needed a lock around an operation that has no shared mutable
/// state, and every encode-only caller still paid for a scratch buffer it never
/// touched.
///
/// Handing the workspace in at the call site instead makes every method here
/// `&self`, so one `Codec` serves any number of concurrent decoders, each with
/// its own scratch and no lock between them:
///
/// ```
/// use reed_solomon_engine::{Codec, ShardMask};
/// use std::sync::Arc;
///
/// let codec = Arc::new(Codec::new(4, 2)?);
///
/// let handles: Vec<_> = (0..4)
///     .map(|_| {
///         let codec = Arc::clone(&codec);
///         std::thread::spawn(move || {
///             // Per-thread scratch; the shared matrix is only ever read.
///             let mut workspace = codec.workspace();
///             let mut shards: Vec<Vec<u8>> =
///                 (0..6).map(|i| vec![i as u8; 64]).collect();
///             codec.encode_all(&mut shards).unwrap();
///
///             let mut present = ShardMask::all_present(6);
///             present.remove(2);
///             codec.reconstruct(&mut shards, &present, &mut workspace).unwrap();
///             shards[2].clone()
///         })
///     })
///     .collect();
///
/// for handle in handles {
///     assert_eq!(handle.join().unwrap(), vec![2u8; 64]);
/// }
/// # Ok::<(), reed_solomon_engine::Error>(())
/// ```
///
/// [`workspace`](Self::workspace) allocates a correctly sized buffer, so the
/// ergonomic cost of passing it is one line. On a target without an allocator,
/// use [`ParityMatrix`](crate::ParityMatrix) or
/// [`ParityMatrixRef`](crate::ParityMatrixRef), which take the same workspace
/// the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codec {
    data_shards: usize,
    parity_shards: usize,
    parity_matrix: Box<[u8]>,
    kernel: Kernel,
}

impl Codec {
    /// Builds a codec for `data_shards` data shards and `parity_shards` parity
    /// shards.
    ///
    /// Both must be at least 1 and their sum must not exceed
    /// [`MAX_SHARDS`](crate::MAX_SHARDS).
    pub fn new(data_shards: usize, parity_shards: usize) -> Result<Self, Error> {
        Self::with_kernel(Kernel::detect(), data_shards, parity_shards)
    }

    /// Builds a codec that uses a specific inner kernel.
    ///
    /// [`new`](Self::new) picks the fastest kernel the CPU supports, which is
    /// what you want. This exists for benchmarking one kernel against another
    /// and for pinning [`Kernel::SCALAR`] when you want the reference path.
    /// Output bytes are identical either way.
    pub fn with_kernel(
        kernel: Kernel,
        data_shards: usize,
        parity_shards: usize,
    ) -> Result<Self, Error> {
        driver::check_counts(data_shards, parity_shards)?;

        let mut parity_matrix = vec![0u8; parity_shards * data_shards].into_boxed_slice();
        if !matrix::build_parity_matrix(data_shards, parity_shards, &mut parity_matrix) {
            return Err(Error::SingularMatrix);
        }

        Ok(Self { data_shards, parity_shards, parity_matrix, kernel })
    }

    /// Writes the parity shards for `data` into `parity`.
    ///
    /// `data` must hold [`data_shards`](Self::data_shards) shards and `parity`
    /// [`parity_shards`](Self::parity_shards) of them, all the same non-zero
    /// length. The previous contents of `parity` are ignored.
    ///
    /// Both arguments are slices of anything that is a byte slice, so
    /// `&[&[u8]]`, `&[Vec<u8>]`, `&[[u8; N]]` and `&[Box<[u8]>]` all work
    /// directly, with no adapter slice built per call.
    ///
    /// Never allocates.
    pub fn encode<D, P>(&self, data: &[D], parity: &mut [P]) -> Result<(), Error>
    where
        D: AsRef<[u8]>,
        P: AsRef<[u8]> + AsMut<[u8]>,
    {
        self.as_matrix_ref().encode(data, parity)
    }

    /// Writes the parity shards for a single slice holding the data shards
    /// followed by the parity shards.
    ///
    /// This is the layout [`reconstruct`](Self::reconstruct) expects, so a
    /// caller that keeps one shard buffer can use it for both directions.
    ///
    /// Never allocates.
    pub fn encode_all<S: AsRef<[u8]> + AsMut<[u8]>>(&self, shards: &mut [S]) -> Result<(), Error> {
        self.as_matrix_ref().encode_all(shards)
    }

    /// Reconstructs every missing shard in place.
    ///
    /// `shards` holds the data shards followed by the parity shards. Shards
    /// marked missing in `present` are treated as scratch: their contents are
    /// ignored and overwritten. All shards must have the same non-zero length,
    /// including the missing ones. `workspace` must be at least
    /// [`workspace_len`](Self::workspace_len) bytes, which
    /// [`workspace`](Self::workspace) provides, and its contents on entry are
    /// ignored.
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
        self.as_matrix_ref().reconstruct(shards, present, workspace)
    }

    /// Reconstructs only the missing data shards, leaving missing parity shards
    /// holding whatever they held on entry.
    ///
    /// Prefer this when the parity shards are about to be discarded: it saves
    /// [`data_shards`](Self::data_shards) shard operations per missing parity
    /// shard.
    ///
    /// Never allocates.
    pub fn reconstruct_data<S: AsRef<[u8]> + AsMut<[u8]>>(
        &self,
        shards: &mut [S],
        present: &ShardMask,
        workspace: &mut [u8],
    ) -> Result<(), Error> {
        self.as_matrix_ref().reconstruct_data(shards, present, workspace)
    }

    /// A zeroed workspace of the right size for this codec's
    /// [`reconstruct`](Self::reconstruct) and
    /// [`reconstruct_data`](Self::reconstruct_data).
    ///
    /// Allocate one per decoding thread and reuse it across calls; the contents
    /// carry nothing between operations.
    #[inline]
    pub fn workspace(&self) -> Vec<u8> {
        vec![0u8; self.workspace_len()]
    }

    /// Bytes of workspace [`reconstruct`](Self::reconstruct) needs.
    #[inline]
    pub const fn workspace_len(&self) -> usize {
        driver::workspace_len(self.data_shards, self.parity_shards)
    }

    /// The `parity_shards * data_shards` row-major generator coefficients.
    ///
    /// Exposed for known-answer tests and for callers that want to persist the
    /// matrix and later reload it into a
    /// [`ParityMatrixRef`](crate::ParityMatrixRef) on a target with no
    /// allocator.
    #[inline]
    pub fn parity_matrix(&self) -> &[u8] {
        &self.parity_matrix
    }

    /// Borrows the coefficients and the split as a [`ParityMatrixRef`].
    ///
    /// Every operation above goes through this, so all four entry points in the
    /// crate run the same checked code. It is public because the borrowed form
    /// is what the allocation-free API takes, so this is the seam between the
    /// two layers rather than an implementation detail.
    ///
    /// The narrowing casts are lossless: [`with_kernel`](Self::with_kernel) put
    /// both counts through the same check that caps their sum at
    /// [`MAX_SHARDS`](crate::MAX_SHARDS).
    #[inline]
    pub fn as_matrix_ref(&self) -> ParityMatrixRef<'_> {
        ParityMatrixRef::new_unchecked(
            self.data_shards as u16,
            self.parity_shards as u16,
            &self.parity_matrix,
        )
        .with_kernel(self.kernel)
    }

    /// The inner kernel this codec uses.
    #[inline]
    pub const fn kernel(&self) -> Kernel {
        self.kernel
    }

    /// Number of data shards.
    #[inline]
    pub const fn data_shards(&self) -> usize {
        self.data_shards
    }

    /// Number of parity shards.
    #[inline]
    pub const fn parity_shards(&self) -> usize {
        self.parity_shards
    }

    /// Total number of shards, data plus parity.
    #[inline]
    pub const fn total_shards(&self) -> usize {
        self.data_shards + self.parity_shards
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_codec_reports_the_split_it_was_built_for() {
        let codec = Codec::new(4, 2).unwrap();
        assert_eq!(codec.data_shards(), 4);
        assert_eq!(codec.parity_shards(), 2);
        assert_eq!(codec.total_shards(), 6);
        assert_eq!(codec.workspace_len(), driver::workspace_len(4, 2));
        assert_eq!(codec.workspace().len(), codec.workspace_len());
    }

    #[test]
    fn a_zero_count_or_an_oversized_total_is_refused() {
        assert_eq!(Codec::new(0, 2), Err(Error::NoDataShards));
        assert_eq!(Codec::new(4, 0), Err(Error::NoParityShards));
        assert_eq!(Codec::new(200, 57), Err(Error::TooManyShards { total: 257 }));
    }

    #[test]
    fn the_borrowed_view_agrees_with_the_owned_codec() {
        let codec = Codec::new(4, 2).unwrap();
        let view = codec.as_matrix_ref();
        assert_eq!(view.coefficients(), codec.parity_matrix());
        assert_eq!(view.data_shards(), codec.data_shards());
        assert_eq!(view.parity_shards(), codec.parity_shards());
        assert_eq!(view.workspace_len(), codec.workspace_len());
    }

    /// The point of the redesign: one shared codec, decoding concurrently.
    #[test]
    fn a_shared_codec_decodes_through_a_shared_reference() {
        let codec = Codec::new(4, 2).unwrap();
        let decode_once = |seed: u8| {
            let mut workspace = codec.workspace();
            let mut shards: Vec<Vec<u8>> = (0..6).map(|i| vec![seed.wrapping_add(i); 32]).collect();
            codec.encode_all(&mut shards).unwrap();
            let expected = shards[1].clone();

            let mut present = ShardMask::all_present(6);
            present.remove(1);
            shards[1].fill(0xFF);
            // `&codec`, not `&mut codec`: this is what 0.1.0 could not express.
            codec.reconstruct(&mut shards, &present, &mut workspace).unwrap();
            assert_eq!(shards[1], expected);
        };
        decode_once(0);
        decode_once(100);
    }

    /// Every container shape the blanket bounds admit reaches the same bytes.
    #[test]
    fn owned_borrowed_and_array_containers_agree() {
        let codec = Codec::new(4, 2).unwrap();
        let data: [[u8; 5]; 4] = [*b"alpha", *b"bravo", *b"charl", *b"delta"];

        // Arrays of arrays.
        let mut from_arrays = [[0u8; 5]; 2];
        codec.encode(&data, &mut from_arrays).unwrap();

        // Owned vectors, which previously needed two adapter slices.
        let owned: Vec<Vec<u8>> = data.iter().map(|d| d.to_vec()).collect();
        let mut from_owned: Vec<Vec<u8>> = vec![vec![0; 5]; 2];
        codec.encode(&owned, &mut from_owned).unwrap();

        // Boxed slices.
        let boxed: Vec<Box<[u8]>> = data.iter().map(|d| d.to_vec().into()).collect();
        let mut from_boxed: Vec<Box<[u8]>> = vec![vec![0u8; 5].into(), vec![0u8; 5].into()];
        codec.encode(&boxed, &mut from_boxed).unwrap();

        // Borrowed slices, the 0.1.0 shape.
        let refs: [&[u8]; 4] = [&data[0], &data[1], &data[2], &data[3]];
        let (mut p0, mut p1) = ([0u8; 5], [0u8; 5]);
        let mut from_refs: [&mut [u8]; 2] = [&mut p0, &mut p1];
        codec.encode(&refs, &mut from_refs).unwrap();

        for (row, expected) in from_arrays.iter().enumerate() {
            assert_eq!(&from_owned[row][..], expected);
            assert_eq!(&from_boxed[row][..], expected);
        }
        assert_eq!(&p0, &from_arrays[0]);
        assert_eq!(&p1, &from_arrays[1]);
    }

    #[test]
    fn reconstruct_data_leaves_missing_parity_untouched() {
        let codec = Codec::new(4, 2).unwrap();
        let mut workspace = codec.workspace();
        let mut shards: Vec<Vec<u8>> = vec![
            b"alpha".to_vec(),
            b"bravo".to_vec(),
            b"charl".to_vec(),
            b"delta".to_vec(),
            vec![0; 5],
            vec![0; 5],
        ];
        codec.encode_all(&mut shards).unwrap();

        let mut present = ShardMask::all_present(6);
        present.remove(0);
        present.remove(5);
        shards[0].fill(0xA5);
        shards[5].fill(0xA5);

        codec.reconstruct_data(&mut shards, &present, &mut workspace).unwrap();
        assert_eq!(shards[0], b"alpha");
        // Parity 5 was not asked for and so was not recomputed.
        assert_eq!(shards[5], vec![0xA5; 5]);
    }

    #[test]
    fn too_few_shards_is_refused() {
        let codec = Codec::new(4, 2).unwrap();
        let mut workspace = codec.workspace();
        let mut shards: Vec<Vec<u8>> = (0..6).map(|_| vec![0u8; 8]).collect();

        let mut present = ShardMask::all_present(6);
        present.remove(0);
        present.remove(1);
        present.remove(2);
        assert_eq!(
            codec.reconstruct(&mut shards, &present, &mut workspace),
            Err(Error::TooFewShards { present: 3, required: 4 })
        );
    }

    #[test]
    fn a_workspace_that_is_too_small_is_refused() {
        let codec = Codec::new(4, 2).unwrap();
        let mut shards: Vec<Vec<u8>> = (0..6).map(|_| vec![0u8; 8]).collect();
        let mut workspace = vec![0u8; codec.workspace_len() - 1];
        let present = ShardMask::all_present(6);
        assert_eq!(
            codec.reconstruct(&mut shards, &present, &mut workspace),
            Err(Error::WorkspaceTooSmall {
                required: codec.workspace_len(),
                actual: codec.workspace_len() - 1,
            })
        );
    }
}
