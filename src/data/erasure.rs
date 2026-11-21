use crate::error::{ArgosError, Result};
use reed_solomon_engine::{Codec, ShardMask};

/// Backend boundary: ArgosFS owns its shard and error semantics, independently
/// of the implementation of GF(2^8) systematic Vandermonde encoding.
trait StripeCodec {
    fn encode_all(&self, shards: &mut [Vec<u8>]) -> Result<()>;
    fn reconstruct(&self, shards: &mut [Vec<u8>], present: &ShardMask) -> Result<()>;
}

struct VandermondeCodec(Codec);

impl StripeCodec for VandermondeCodec {
    fn encode_all(&self, shards: &mut [Vec<u8>]) -> Result<()> {
        self.0
            .encode_all(shards)
            .map_err(|err| ArgosError::Erasure(err.to_string()))
    }

    fn reconstruct(&self, shards: &mut [Vec<u8>], present: &ShardMask) -> Result<()> {
        let mut workspace = self.0.workspace();
        self.0
            .reconstruct(shards, present, &mut workspace)
            .map_err(|err| ArgosError::Erasure(err.to_string()))
    }
}

/// Wire-compatible GF(2^8)/0x11D codec. The vendored implementation selects
/// AVX2, SSSE3, NEON or scalar only after checking CPU capabilities.
pub struct RsCodec {
    inner: Option<VandermondeCodec>,
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
        // Reed-Solomon over GF(2^8) can address no more than 256 shards,
        // including a parity-free layout.
        if total > reed_solomon_engine::MAX_SHARDS {
            return Err(ArgosError::Invalid(format!(
                "erasure shard count {total} exceeds GF(2^8) limit"
            )));
        }
        let inner = if m == 0 {
            None
        } else {
            Some(VandermondeCodec(
                Codec::new(k, m).map_err(|err| ArgosError::Erasure(err.to_string()))?,
            ))
        };
        Ok(Self { inner, k, m, total })
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn encode(&self, data_shards: &[Vec<u8>]) -> Result<Vec<Vec<u8>>> {
        if data_shards.len() != self.k {
            return Err(ArgosError::Invalid(format!(
                "expected {} data shards, got {}",
                self.k,
                data_shards.len()
            )));
        }
        let shard_size = data_shards.first().map(Vec::len).unwrap_or(0);
        if data_shards.iter().any(|shard| shard.len() != shard_size) {
            return Err(ArgosError::Invalid(
                "data shards must have equal size".to_string(),
            ));
        }
        let mut shards = data_shards.to_vec();
        if self.m == 0 {
            return Ok(shards);
        }
        shards.extend((0..self.m).map(|_| vec![0u8; shard_size]));
        self.inner
            .as_ref()
            .expect("parity layout must have a codec")
            .encode_all(&mut shards)?;
        Ok(shards)
    }

    pub fn reconstruct(&self, shards: Vec<Option<Vec<u8>>>) -> Result<Vec<Vec<u8>>> {
        if shards.len() != self.total {
            return Err(ArgosError::Invalid(format!(
                "expected {} shards, got {}",
                self.total,
                shards.len()
            )));
        }
        if self.m == 0 && shards.iter().any(Option::is_none) {
            return Err(ArgosError::Erasure(
                "layout has no parity shards for reconstruction".to_string(),
            ));
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
                    buffers.push(vec![0; shard_size]);
                }
            }
        }

        if missing {
            self.inner
                .as_ref()
                .expect("parity layout must have a codec")
                .reconstruct(&mut buffers, &present)?;
        }
        Ok(buffers)
    }
}

#[cfg(test)]
#[path = "erasure_tests.rs"]
mod tests;
