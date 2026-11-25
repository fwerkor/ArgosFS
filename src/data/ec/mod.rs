//! ArgosFS-owned systematic GF(256) Reed–Solomon engine.
//! Adapted arithmetic and SIMD kernels from reed-solomon-engine 0.2.0
//! (MIT OR Apache-2.0); see THIRD_PARTY.md and the retained license files.

#![deny(unsafe_code)]
#![forbid(unsafe_op_in_unsafe_fn)]

mod driver;
mod error;
mod gf;
mod kernel;
mod mask;
mod matrix;
mod shards;

pub(super) use driver::DecodeWorkspace;
use error::Error;
pub(super) use error::MAX_SHARDS;
pub(super) use kernel::Kernel;
pub(super) use mask::ShardMask;

/// Immutable geometry and generator coefficients, with runtime-selected SIMD.
pub(super) struct Codec {
    k: usize,
    m: usize,
    matrix: Box<[u8]>,
    kernel: Kernel,
}

impl Codec {
    pub(super) fn new(k: usize, m: usize) -> Result<Self, Error> {
        Self::with_kernel(Kernel::detect(), k, m)
    }

    pub(super) fn with_kernel(kernel: Kernel, k: usize, m: usize) -> Result<Self, Error> {
        driver::check_counts(k, m)?;
        let mut matrix = vec![0; k * m].into_boxed_slice();
        if !matrix::build_parity_matrix(k, m, &mut matrix) {
            return Err(Error::SingularMatrix);
        }
        Ok(Self {
            k,
            m,
            matrix,
            kernel,
        })
    }

    pub(super) fn encode_all(&self, shards: &mut [Vec<u8>]) -> Result<(), Error> {
        driver::encode_all_with_kernel(self.kernel, &self.matrix, self.k, self.m, shards)
    }

    #[cfg(test)]
    pub(super) fn workspace(&self) -> DecodeWorkspace {
        DecodeWorkspace::new()
    }

    pub(super) fn reconstruct(
        &self,
        shards: &mut [Vec<u8>],
        present: &ShardMask,
        workspace: &mut DecodeWorkspace,
    ) -> Result<(), Error> {
        driver::reconstruct_impl(
            self.kernel,
            &self.matrix,
            self.k,
            self.m,
            shards,
            present,
            workspace,
            true,
        )
    }

    pub(super) fn reconstruct_data(
        &self,
        shards: &mut [Vec<u8>],
        present: &ShardMask,
        workspace: &mut DecodeWorkspace,
    ) -> Result<(), Error> {
        driver::reconstruct_impl(
            self.kernel,
            &self.matrix,
            self.k,
            self.m,
            shards,
            present,
            workspace,
            false,
        )
    }
}
