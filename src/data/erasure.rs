use super::ec::{Codec, DecodeWorkspace, ShardMask, MAX_SHARDS};
use crate::error::{ArgosError, Result};
use std::cell::RefCell;

thread_local! {
    // One layout per decoding thread, bounded independently of stripe count.
    // There is no shared mutable codec state between FUSE workers.
    static READ_CODEC: RefCell<Option<(usize, usize, RsCodec)>> = const { RefCell::new(None) };
    // Reuse the last recovery plan and its allocation on each worker.
    static DECODE_WORKSPACE: RefCell<DecodeWorkspace> = const { RefCell::new(DecodeWorkspace::new()) };
}

/// Wire-compatible GF(2^8)/0x11D codec. The native implementation selects
/// GFNI, AVX-512, AVX2, SSSE3, SVE/SVE2, NEON or scalar after checking CPU capabilities.
pub struct RsCodec {
    inner: Option<Codec>,
    k: usize,
    m: usize,
    total: usize,
}

impl RsCodec {
    pub fn new(k: usize, m: usize) -> Result<Self> {
        if k == 0 {
            return Err(ArgosError::Invalid("k must be positive".to_string()));
        }
        let total = k
            .checked_add(m)
            .ok_or_else(|| ArgosError::Invalid("erasure shard count overflow".to_string()))?;
        // Only erasure-coded layouts are bounded by the GF(2^8) field.
        // The parity-free path historically permits more than 256 shards.
        if m != 0 && total > MAX_SHARDS {
            return Err(ArgosError::Invalid(format!(
                "erasure shard count {total} exceeds GF(2^8) limit"
            )));
        }
        let inner = if m == 0 {
            None
        } else {
            Some(Codec::new(k, m).map_err(|err| ArgosError::Erasure(err.to_string()))?)
        };
        Ok(Self { inner, k, m, total })
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn encode(&self, data_shards: &[Vec<u8>]) -> Result<Vec<Vec<u8>>> {
        self.encode_owned(data_shards.to_vec())
    }

    pub(crate) fn encode_owned(&self, mut shards: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>> {
        if shards.len() != self.k {
            return Err(ArgosError::Invalid(format!(
                "expected {} data shards, got {}",
                self.k,
                shards.len()
            )));
        }
        let shard_size = shards.first().map(Vec::len).unwrap_or(0);
        if shards.iter().any(|shard| shard.len() != shard_size) {
            return Err(ArgosError::Invalid(
                "data shards must have equal size".to_string(),
            ));
        }
        if self.m == 0 {
            return Ok(shards);
        }
        shards.extend((0..self.m).map(|_| vec![0u8; shard_size]));
        self.inner
            .as_ref()
            .expect("parity layout must have a codec")
            .encode_all(&mut shards)
            .map_err(|err| ArgosError::Erasure(err.to_string()))?;
        Ok(shards)
    }

    pub fn reconstruct(&self, shards: Vec<Option<Vec<u8>>>) -> Result<Vec<Vec<u8>>> {
        self.reconstruct_inner(shards, false)
    }

    /// Decode data shards only. Reuse the immutable generator matrix and
    /// reconstruction scratch on the current thread, not once per stripe.
    pub(crate) fn reconstruct_data_cached(
        k: usize,
        m: usize,
        shards: Vec<Option<Vec<u8>>>,
    ) -> Result<Vec<Vec<u8>>> {
        Self::reconstruct_cached_inner(k, m, shards, true)
    }

    /// Repair needs all shards, including parity, but can reuse the same
    /// bounded per-thread codec cache as the read-side data-only decoder.
    pub(crate) fn reconstruct_cached(
        k: usize,
        m: usize,
        shards: Vec<Option<Vec<u8>>>,
    ) -> Result<Vec<Vec<u8>>> {
        Self::reconstruct_cached_inner(k, m, shards, false)
    }

    fn reconstruct_cached_inner(
        k: usize,
        m: usize,
        shards: Vec<Option<Vec<u8>>>,
        data_only: bool,
    ) -> Result<Vec<Vec<u8>>> {
        READ_CODEC.with(|cached| {
            let mut cached = cached.borrow_mut();
            if !matches!(cached.as_ref(), Some((old_k, old_m, _)) if *old_k == k && *old_m == m) {
                *cached = Some((k, m, Self::new(k, m)?));
            }
            cached
                .as_ref()
                .unwrap()
                .2
                .reconstruct_inner(shards, data_only)
        })
    }

    fn reconstruct_inner(
        &self,
        shards: Vec<Option<Vec<u8>>>,
        data_only: bool,
    ) -> Result<Vec<Vec<u8>>> {
        if shards.len() != self.total {
            return Err(ArgosError::Invalid(format!(
                "expected {} shards, got {}",
                self.total,
                shards.len()
            )));
        }
        if self.m == 0 {
            // Legacy no-parity layouts return the provided shards unchanged;
            // they do not impose the parity codec's equal-length requirement.
            return shards
                .into_iter()
                .map(|shard| {
                    shard.ok_or_else(|| {
                        ArgosError::Erasure(
                            "layout has no parity shards for reconstruction".to_string(),
                        )
                    })
                })
                .collect();
        }

        let shard_size = shards.iter().find_map(|shard| shard.as_ref().map(Vec::len));
        let Some(shard_size) = shard_size else {
            return Err(ArgosError::Erasure(
                "no shards available for reconstruction".to_string(),
            ));
        };
        if shards
            .iter()
            .flatten()
            .any(|shard| shard.len() != shard_size)
        {
            return Err(ArgosError::Erasure(
                "shards must have equal size".to_string(),
            ));
        }
        // The legacy erase codec rejects zero-sized shards, including when
        // all shards are present; preserve that contract for parity layouts.
        if self.m != 0 && shard_size == 0 {
            return Err(ArgosError::Erasure("shards must not be empty".to_string()));
        }

        if data_only && shards[..self.k].iter().all(Option::is_some) {
            return Ok(shards
                .into_iter()
                .take(self.k)
                .map(Option::unwrap)
                .collect());
        }

        let mut present = ShardMask::new();
        let mut missing = false;
        let mut buffers = Vec::with_capacity(self.total);
        for (i, shard) in shards.into_iter().enumerate() {
            match shard {
                Some(data) => {
                    present.insert(i);
                    buffers.push(data);
                }
                None => {
                    missing = true;
                    buffers.push(if data_only && i >= self.k {
                        Vec::new()
                    } else {
                        vec![0; shard_size]
                    });
                }
            }
        }

        if missing {
            let codec = self
                .inner
                .as_ref()
                .expect("parity layout must have a codec");
            DECODE_WORKSPACE.with(|scratch| {
                let mut workspace = scratch.borrow_mut();
                if data_only {
                    codec.reconstruct_data(&mut buffers, &present, &mut workspace)
                } else {
                    codec.reconstruct(&mut buffers, &present, &mut workspace)
                }
                .map_err(|err| ArgosError::Erasure(err.to_string()))
            })?;
        }
        if data_only {
            buffers.truncate(self.k);
        }
        Ok(buffers)
    }
}

#[cfg(test)]
#[path = "erasure_tests.rs"]
mod tests;
