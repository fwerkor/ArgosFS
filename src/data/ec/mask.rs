//! Fixed-size presence mask for the GF(256) shard indices.
use super::MAX_SHARDS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ShardMask([u64; MAX_SHARDS / 64]);

impl ShardMask {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn insert(&mut self, index: usize) {
        self.0[index / 64] |= 1 << (index % 64);
    }
    pub(crate) fn is_present(&self, index: usize) -> bool {
        self.0[index / 64] & (1 << (index % 64)) != 0
    }
    #[cfg(test)]
    pub(crate) fn all_present(count: usize) -> Self {
        let mut mask = Self::new();
        for i in 0..count {
            mask.insert(i);
        }
        mask
    }
    #[cfg(test)]
    pub(crate) fn remove(&mut self, index: usize) {
        self.0[index / 64] &= !(1 << (index % 64));
    }
}
