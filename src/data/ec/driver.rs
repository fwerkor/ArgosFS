//! Checked encoding and erasure recovery on caller-owned shard buffers.

use crate::data::ec::error::{Error, MAX_SHARDS};
use crate::data::ec::kernel::Kernel;
use crate::data::ec::mask::ShardMask;
use crate::data::ec::matrix;
use crate::data::ec::shards::ShardSource;

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
    if coeffs.is_empty() {
        out.fill(0);
        return;
    }
    if coeffs.len() == 2
        && kernel.try_mul_two(
            [coeffs[0], coeffs[1]],
            [
                inputs.shard(indices[0] as usize),
                inputs.shard(indices[1] as usize),
            ],
            out,
        )
    {
        return;
    }
    // Retain the output tile in cache while combining all surviving inputs.
    const TILE: usize = 64 * 1024;
    for start in (0..out.len()).step_by(TILE) {
        let end = (start + TILE).min(out.len());
        let dst = &mut out[start..end];
        kernel.mul_slice(
            coeffs[0],
            &inputs.shard(indices[0] as usize)[start..end],
            dst,
        );
        for (&c, &index) in coeffs.iter().zip(indices).skip(1) {
            kernel.mul_add_slice(c, &inputs.shard(index as usize)[start..end], dst);
        }
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
    let (out, right) = rest
        .split_first_mut()
        .expect("out_index is always a valid shard index");
    let inputs = SplitShards {
        left: &*left,
        right: &*right,
        gap: out_index,
    };
    combine(kernel, coeffs, indices, &inputs, out.as_mut());
}

/// Validate the codec geometry.
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
        return Err(Error::MatrixSizeMismatch {
            expected,
            actual: parity_matrix.len(),
        });
    }
    Ok(())
}

/// Encode a single slice holding the data shards followed
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
        return Err(Error::ShardCountMismatch {
            expected: total,
            actual: shards.len(),
        });
    }

    uniform_shard_len(&*shards)?;

    let (data, parity) = shards.split_at_mut(data_shards);
    // As in `encode_with_kernel`: one blocked call over every parity row, not a
    // row-major loop.
    kernel.mul_rows(parity_matrix, data_shards, &*data, parity);
    Ok(())
}

/// One recovery plan per worker. The generator matrix is deterministic for
/// the geometry, so the same presence mask can reuse the reduced system.
/// Memory is proportional to the largest actual erasure count seen by the
/// worker, not the stripe size or the number of stripes.
pub(crate) struct DecodeWorkspace {
    key: Option<(usize, usize, ShardMask)>,
    system: Vec<u8>,
    #[cfg(test)]
    rebuilds: usize,
}

impl DecodeWorkspace {
    pub(crate) const fn new() -> Self {
        Self {
            key: None,
            system: Vec::new(),
            #[cfg(test)]
            rebuilds: 0,
        }
    }
}

/// The decode algorithm.
///
/// Let `M` be the set of missing data shards, `e = |M|`, and pick any `e`
/// surviving parity shards with matrix rows `r_0..r_{e-1}`. Each such row gives
/// one linear equation in the unknown data shards:
///
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
    workspace: &mut DecodeWorkspace,
    regenerate_parity: bool,
) -> Result<(), Error> {
    check_counts(data_shards, parity_shards)?;
    check_matrix(parity_matrix, data_shards, parity_shards)?;
    let total = data_shards + parity_shards;
    if shards.len() != total {
        return Err(Error::ShardCountMismatch {
            expected: total,
            actual: shards.len(),
        });
    }

    // Missing parity is not allocated on data-only reads. All actual
    // inputs and outputs still have to share the same nonzero length.
    let len = shards[0].as_ref().len();
    if len == 0 {
        return Err(Error::EmptyShard);
    }
    for (i, shard) in shards.iter().enumerate() {
        if (regenerate_parity || i < data_shards || present.is_present(i))
            && shard.as_ref().len() != len
        {
            return Err(Error::ShardLengthMismatch);
        }
    }

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
        return Err(Error::TooFewShards {
            present: n_present,
            required: data_shards,
        });
    }

    let missing = n_missing_data;
    if missing > 0 {
        // Inputs: every surviving data shard, then the parity shards whose
        // equations we use. That is exactly `data_shards` inputs.
        let mut inputs = [0u8; MAX_SHARDS];
        inputs[..n_present_data].copy_from_slice(&present_data[..n_present_data]);
        inputs[n_present_data..data_shards].copy_from_slice(&present_parity[..missing]);

        let cols = missing + data_shards;
        let key = (data_shards, parity_shards, *present);
        if workspace.key != Some(key) {
            // Invalidate before mutating: no failed solve can poison a later hit.
            workspace.key = None;
            workspace.system.resize(missing * cols, 0);
            let system = &mut workspace.system[..missing * cols];
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

            // Pack the right-hand side once per cache miss. The identity
            // columns are no longer needed; dense rows permit fused recovery.
            for row in 0..missing {
                system.copy_within(row * cols + missing..(row + 1) * cols, row * data_shards);
            }
            workspace.system.truncate(missing * data_shards);
            workspace.key = Some(key);
            #[cfg(test)]
            {
                workspace.rebuilds += 1;
            }
        }
        let system = &workspace.system;
        let input_indices = &inputs[..data_shards];
        if missing == 1 {
            combine_in_place(
                kernel,
                shards,
                missing_data[0] as usize,
                system,
                input_indices,
            );
        } else {
            // Inputs and outputs are sorted, disjoint indices. Borrow each
            // shard exactly once, with no payload copies or heap allocation.
            let mut sources: [&[u8]; MAX_SHARDS] = [&[]; MAX_SHARDS];
            let mut outputs: [&mut [u8]; MAX_SHARDS] = core::array::from_fn(|_| &mut [][..]);
            let mut input = 0;
            let mut output = 0;
            for (i, shard) in shards.iter_mut().enumerate() {
                if input_indices.get(input).copied() == Some(i as u8) {
                    sources[input] = <S as AsRef<[u8]>>::as_ref(&*shard);
                    input += 1;
                } else if missing_data[..missing].get(output).copied() == Some(i as u8) {
                    outputs[output] = <S as AsMut<[u8]>>::as_mut(shard);
                    output += 1;
                }
            }
            debug_assert_eq!(input, data_shards);
            debug_assert_eq!(output, missing);
            kernel.mul_rows(
                system,
                data_shards,
                &sources[..input],
                &mut outputs[..output],
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::ec::Codec;

    #[test]
    fn two_input_codec_matches_reference_at_vector_and_tile_boundaries() {
        for len in [31, 32, 33, 4095, 4096, 65535, 65536, 65537, 1048576] {
            let reference = Codec::with_kernel(Kernel::SCALAR, 2, 1).unwrap();
            let mut original: Vec<Vec<u8>> = (0..3)
                .map(|j| {
                    (0..len)
                        .map(|i| (i * 17 + j * 41 + i / 251) as u8)
                        .collect()
                })
                .collect();
            reference.encode_all(&mut original).unwrap();
            for kernel in Kernel::supported() {
                let codec = Codec::with_kernel(kernel, 2, 1).unwrap();
                let mut shards = original.clone();
                shards[2].fill(0x77);
                codec.encode_all(&mut shards).unwrap();
                assert_eq!(shards, original, "{kernel}: encode length {len}");
                for missing in 0..2 {
                    let mut present = ShardMask::all_present(3);
                    present.remove(missing);
                    shards[missing].fill(0x55);
                    codec
                        .reconstruct_data(&mut shards, &present, &mut DecodeWorkspace::new())
                        .unwrap();
                    assert_eq!(shards, original, "{kernel}: recover length {len}");
                }
            }
        }
    }

    #[test]
    fn fused_recovery_matches_reference_for_all_supported_kernels() {
        for (k, m) in [(4, 2), (8, 4), (16, 8)] {
            let reference = Codec::with_kernel(Kernel::SCALAR, k, m).unwrap();
            let mut original: Vec<_> = (0..k + m)
                .map(|j| (0..4173).map(|i| (i * 17 + j * 41) as u8).collect())
                .collect();
            reference.encode_all(&mut original).unwrap();
            for kernel in Kernel::supported() {
                let codec = Codec::with_kernel(kernel, k, m).unwrap();
                let mut workspace = DecodeWorkspace::new();
                for missing in [2, m] {
                    let mut present = ShardMask::all_present(k + m);
                    let mut shards = original.clone();
                    for j in 0..missing {
                        let index = j * k / missing;
                        present.remove(index);
                        shards[index].fill(0x77);
                    }
                    for _ in 0..2 {
                        codec
                            .reconstruct_data(&mut shards, &present, &mut workspace)
                            .unwrap();
                        assert_eq!(shards, original, "{kernel}: {k}+{m}, {missing} erasures");
                    }
                }
            }
        }
    }

    #[test]
    fn recovery_plan_is_reused_and_keyed_by_geometry_and_presence() {
        let mut workspace = DecodeWorkspace::new();
        for (k, m, missing, expected_rebuilds) in [
            (4, 2, 1, 1),
            (4, 2, 1, 1),
            (4, 2, 2, 2),
            (3, 3, 2, 3),
            (3, 3, 2, 3),
            (4, 2, 2, 4),
        ] {
            let codec = Codec::new(k, m).unwrap();
            let mut shards: Vec<_> = (0..k + m).map(|i| vec![i as u8 + 7; 131]).collect();
            codec.encode_all(&mut shards).unwrap();
            let expected = shards.clone();
            let mut present = ShardMask::all_present(k + m);
            present.remove(missing);
            present.remove(k + m - 1);
            shards[missing].fill(0xa5);
            shards[k + m - 1].clear(); // absent parity has no allocation
            codec
                .reconstruct_data(&mut shards, &present, &mut workspace)
                .unwrap();
            assert_eq!(&shards[..k], &expected[..k]);
            assert!(shards[k + m - 1].is_empty());
            assert_eq!(workspace.rebuilds, expected_rebuilds);
        }
    }
}
