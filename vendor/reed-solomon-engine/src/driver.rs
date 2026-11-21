//! Allocation-free encode and reconstruct primitives.
//!
//! These take the parity matrix as a plain `&[u8]` plus the shard split it was
//! built for, and never allocate, so they work in `no_std` without an
//! allocator. They are crate-internal: the public entry point is
//! [`ParityMatrixRef`](crate::ParityMatrixRef), which pairs the coefficients
//! with the split so it cannot be restated inconsistently, and
//! [`ParityMatrix`](crate::ParityMatrix) and [`Codec`](crate::Codec) both reach
//! these functions through it.
//!
//! Every shard-count check in the crate lives here, once, for that reason.

use crate::error::{Error, MAX_SHARDS};
use crate::kernel::Kernel;
use crate::mask::ShardMask;
use crate::matrix;
use crate::shards::ShardSource;

/// `[0, 1, 2, ..., 255]`, used as an identity index list.
const SEQUENTIAL: [u8; MAX_SHARDS] = {
    let mut table = [0u8; MAX_SHARDS];
    let mut i = 0;
    while i < MAX_SHARDS {
        table[i] = i as u8;
        i += 1;
    }
    table
};

/// A shard slice with the element at `gap` removed, so that element can be
/// borrowed mutably as an output while the rest are read as inputs.
struct SplitShards<'a, E> {
    left: &'a [E],
    right: &'a [E],
    gap: usize,
}

impl<E: AsRef<[u8]>> ShardSource for SplitShards<'_, E> {
    #[inline]
    fn count(&self) -> usize {
        self.left.len() + 1 + self.right.len()
    }
    #[inline]
    fn shard(&self, index: usize) -> &[u8] {
        debug_assert_ne!(index, self.gap, "the split-out shard cannot be an input");
        if index < self.gap {
            self.left[index].as_ref()
        } else {
            self.right[index - self.gap - 1].as_ref()
        }
    }
}

/// Returns the common length of every shard, or an error if they differ or are
/// empty.
fn uniform_shard_len<S: ShardSource + ?Sized>(shards: &S) -> Result<usize, Error> {
    let len = shards.shard(0).len();
    if len == 0 {
        return Err(Error::EmptyShard);
    }
    for i in 1..shards.count() {
        if shards.shard(i).len() != len {
            return Err(Error::ShardLengthMismatch);
        }
    }
    Ok(len)
}

/// Computes one output shard as `out = sum over j of coeffs[j] * inputs[indices[j]]`.
///
/// `coeffs` and `indices` must have equal length, and `out` must match the
/// shard length.
fn combine<S: ShardSource + ?Sized>(
    kernel: Kernel,
    coeffs: &[u8],
    indices: &[u8],
    inputs: &S,
    out: &mut [u8],
) {
    debug_assert_eq!(coeffs.len(), indices.len());
    let mut terms = coeffs.iter().zip(indices);
    match terms.next() {
        // The first term is assigned rather than accumulated, which saves
        // zeroing the output.
        Some((&c, &index)) => kernel.mul_slice(c, inputs.shard(index as usize), out),
        None => {
            out.fill(0);
            return;
        }
    }
    for (&c, &index) in terms {
        kernel.mul_add_slice(c, inputs.shard(index as usize), out);
    }
}

/// Same as [`combine`], but the output lives inside `shards` at `out_index`.
///
/// `indices` must not contain `out_index`.
fn combine_in_place<S: AsRef<[u8]> + AsMut<[u8]>>(
    kernel: Kernel,
    shards: &mut [S],
    out_index: usize,
    coeffs: &[u8],
    indices: &[u8],
) {
    let (left, rest) = shards.split_at_mut(out_index);
    let (out, right) = rest.split_first_mut().expect("out_index is always a valid shard index");
    let inputs = SplitShards { left: &*left, right: &*right, gap: out_index };
    combine(kernel, coeffs, indices, &inputs, out.as_mut());
}

/// Validates shard counts shared by every entry point.
///
/// `const` so that [`ParityMatrixRef::new`](crate::ParityMatrixRef::new), which
/// has to run in a `const` context, can call this rather than restate it. Two
/// copies of a check that must agree are exactly the drift this module exists
/// to remove.
pub(crate) const fn check_counts(data_shards: usize, parity_shards: usize) -> Result<(), Error> {
    if data_shards == 0 {
        return Err(Error::NoDataShards);
    }
    if parity_shards == 0 {
        return Err(Error::NoParityShards);
    }
    // Saturating rather than plain addition: `Codec::new` and `Codec::with_kernel`
    // take raw `usize` counts, so a caller can hand in values whose sum wraps
    // past `MAX_SHARDS` back into the legal range. That would let absurd counts
    // through this gate and blow up later in an allocation. Saturation reports
    // them as what they are, oversized.
    let total = data_shards.saturating_add(parity_shards);
    if total > MAX_SHARDS {
        return Err(Error::TooManyShards { total });
    }
    Ok(())
}

/// Checks that a coefficient slice holds exactly one byte per matrix entry.
///
/// This is all a byte slice structurally admits: it fixes the *area* of the
/// matrix, never its shape. `const` for the same reason as [`check_counts`].
pub(crate) const fn check_matrix(
    parity_matrix: &[u8],
    data_shards: usize,
    parity_shards: usize,
) -> Result<(), Error> {
    // Cannot overflow: `check_counts` runs first at every call site and bounds
    // both counts by `MAX_SHARDS`.
    let expected = parity_shards * data_shards;
    if parity_matrix.len() != expected {
        return Err(Error::MatrixSizeMismatch { expected, actual: parity_matrix.len() });
    }
    Ok(())
}

/// Computes the parity shards for `data`.
///
/// The shape is `data_shards + parity_shards`, taken from the matrix rather
/// than inferred from the slices, so `data.len()` and `parity.len()` are
/// checked against it instead of defining it. That is the whole difference
/// between this and the free function it replaces: a caller can no longer
/// silently redefine the code by handing over a differently sized slice.
///
/// Never allocates.
pub(crate) fn encode_with_kernel<D, P>(
    kernel: Kernel,
    parity_matrix: &[u8],
    data_shards: usize,
    parity_shards: usize,
    data: &[D],
    parity: &mut [P],
) -> Result<(), Error>
where
    D: AsRef<[u8]>,
    P: AsRef<[u8]> + AsMut<[u8]>,
{
    check_counts(data_shards, parity_shards)?;
    check_matrix(parity_matrix, data_shards, parity_shards)?;
    if data.len() != data_shards {
        return Err(Error::ShardCountMismatch { expected: data_shards, actual: data.len() });
    }
    if parity.len() != parity_shards {
        return Err(Error::ShardCountMismatch { expected: parity_shards, actual: parity.len() });
    }

    let len = uniform_shard_len(data)?;
    for shard in parity.iter() {
        if shard.as_ref().len() != len {
            return Err(Error::ShardLengthMismatch);
        }
    }

    // Every parity row reads every data shard, so this is one call rather than
    // a loop: the kernel is free to hold several rows' accumulators at once and
    // split each source byte's nibbles once for all of them. See
    // `Kernel::mul_rows`.
    kernel.mul_rows(parity_matrix, data_shards, data, parity);
    Ok(())
}

/// [`encode_with_kernel`] over a single slice holding the data shards followed
/// by the parity shards.
///
/// Never allocates.
pub(crate) fn encode_all_with_kernel<S: AsRef<[u8]> + AsMut<[u8]>>(
    kernel: Kernel,
    parity_matrix: &[u8],
    data_shards: usize,
    parity_shards: usize,
    shards: &mut [S],
) -> Result<(), Error> {
    check_counts(data_shards, parity_shards)?;
    check_matrix(parity_matrix, data_shards, parity_shards)?;
    let total = data_shards + parity_shards;
    if shards.len() != total {
        return Err(Error::ShardCountMismatch { expected: total, actual: shards.len() });
    }

    uniform_shard_len(&*shards)?;

    let (data, parity) = shards.split_at_mut(data_shards);
    // As in `encode_with_kernel`: one blocked call over every parity row, not a
    // row-major loop.
    kernel.mul_rows(parity_matrix, data_shards, &*data, parity);
    Ok(())
}

/// Bytes of workspace reconstruction needs.
///
/// The bound is `parity_shards * (parity_shards + data_shards)`: the decoder
/// solves an `e x e` system where `e` is the number of missing *data* shards,
/// augmented with one column per surviving input.
///
/// This stays a free function rather than becoming only a method because a
/// workspace has to be sized before a matrix exists. A `no_std` caller that
/// loads its coefficients at runtime needs the buffer already reserved by the
/// time it has something to call
/// [`ParityMatrixRef::workspace_len`](crate::ParityMatrixRef::workspace_len)
/// on.
#[inline]
pub const fn workspace_len(data_shards: usize, parity_shards: usize) -> usize {
    parity_shards * (parity_shards + data_shards)
}

/// The decode algorithm.
///
/// Let `M` be the set of missing data shards, `e = |M|`, and pick any `e`
/// surviving parity shards with matrix rows `r_0..r_{e-1}`. Each such row gives
/// one linear equation in the unknown data shards:
///
/// ```text
///   sum over u in M of P[r_t][u] * d_u  =  p_{r_t}  +  sum over present i of P[r_t][i] * d_i
/// ```
///
/// Written as an augmented system `[A | C]` where `A[t][u] = P[r_t][M[u]]` and
/// `C` expresses the right-hand side as a combination of the surviving shards,
/// Gauss-Jordan reduction of `A` to the identity turns `C` into `A^-1 C`: the
/// recovery matrix. Each missing data shard is then a linear combination of
/// surviving shards only, so nothing has to be staged through scratch buffers.
///
/// `A` is non-singular because the code is MDS (see
/// [`matrix::build_parity_matrix`]), and `e <= parity_shards` surviving parity
/// shards always exist whenever at least `data_shards` shards are present.
///
/// The cost of the linear algebra is `O(e^3)`, not `O(data_shards^3)`, which
/// is why losing two shards out of eighty is cheap.
#[allow(clippy::too_many_arguments)]
pub(crate) fn reconstruct_impl<S: AsRef<[u8]> + AsMut<[u8]>>(
    kernel: Kernel,
    parity_matrix: &[u8],
    data_shards: usize,
    parity_shards: usize,
    shards: &mut [S],
    present: &ShardMask,
    workspace: &mut [u8],
    regenerate_parity: bool,
) -> Result<(), Error> {
    check_counts(data_shards, parity_shards)?;
    check_matrix(parity_matrix, data_shards, parity_shards)?;
    let total = data_shards + parity_shards;
    if shards.len() != total {
        return Err(Error::ShardCountMismatch { expected: total, actual: shards.len() });
    }

    let required = workspace_len(data_shards, parity_shards);
    if workspace.len() < required {
        return Err(Error::WorkspaceTooSmall { required, actual: workspace.len() });
    }
    uniform_shard_len(&*shards)?;

    // Partition the shard indices. Each list is bounded by MAX_SHARDS and an
    // index fits in a u8, so these are cheap fixed-size stack arrays.
    let mut present_data = [0u8; MAX_SHARDS];
    let mut missing_data = [0u8; MAX_SHARDS];
    let mut present_parity = [0u8; MAX_SHARDS];
    let mut missing_parity = [0u8; MAX_SHARDS];
    let (mut n_present_data, mut n_missing_data) = (0usize, 0usize);
    let (mut n_present_parity, mut n_missing_parity) = (0usize, 0usize);

    for index in 0..total {
        match (index < data_shards, present.is_present(index)) {
            (true, true) => {
                present_data[n_present_data] = index as u8;
                n_present_data += 1;
            }
            (true, false) => {
                missing_data[n_missing_data] = index as u8;
                n_missing_data += 1;
            }
            (false, true) => {
                present_parity[n_present_parity] = index as u8;
                n_present_parity += 1;
            }
            (false, false) => {
                missing_parity[n_missing_parity] = index as u8;
                n_missing_parity += 1;
            }
        }
    }

    let n_present = n_present_data + n_present_parity;
    if n_present < data_shards {
        return Err(Error::TooFewShards { present: n_present, required: data_shards });
    }

    let missing = n_missing_data;
    if missing > 0 {
        // Inputs: every surviving data shard, then the parity shards whose
        // equations we use. That is exactly `data_shards` inputs.
        let mut inputs = [0u8; MAX_SHARDS];
        inputs[..n_present_data].copy_from_slice(&present_data[..n_present_data]);
        inputs[n_present_data..data_shards].copy_from_slice(&present_parity[..missing]);

        let cols = missing + data_shards;
        let system = &mut workspace[..missing * cols];
        for t in 0..missing {
            let row = present_parity[t] as usize - data_shards;
            let p = &parity_matrix[row * data_shards..(row + 1) * data_shards];
            let out = &mut system[t * cols..(t + 1) * cols];

            for (u, &index) in missing_data[..missing].iter().enumerate() {
                out[u] = p[index as usize];
            }
            for (j, &index) in present_data[..n_present_data].iter().enumerate() {
                out[missing + j] = p[index as usize];
            }
            for j in 0..missing {
                out[missing + n_present_data + j] = u8::from(j == t);
            }
        }

        if !matrix::gauss_jordan(system, missing, cols) {
            return Err(Error::SingularMatrix);
        }

        let input_indices = &inputs[..data_shards];
        for (u, &out_index) in missing_data[..missing].iter().enumerate() {
            let coeffs = &system[u * cols + missing..(u + 1) * cols];
            combine_in_place(kernel, shards, out_index as usize, coeffs, input_indices);
        }
    }

    if regenerate_parity {
        let indices = &SEQUENTIAL[..data_shards];
        for &out_index in &missing_parity[..n_missing_parity] {
            let row = out_index as usize - data_shards;
            let coeffs = &parity_matrix[row * data_shards..(row + 1) * data_shards];
            combine_in_place(kernel, shards, out_index as usize, coeffs, indices);
        }
    }

    Ok(())
}
