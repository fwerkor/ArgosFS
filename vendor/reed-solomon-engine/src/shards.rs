//! Read-only access to a set of equal-length shards.
//!
//! This lives in its own module rather than beside the drivers in
//! [`crate::driver`] because of the layering in `docs/DESIGN.md` section 2. The
//! kernel is the bottom layer, and [`Kernel::mul_rows`](crate::Kernel) reads
//! `data_shards` source shards at once, so it needs this trait. Defining it in
//! `driver` would have the bottom layer importing from the top one, which is
//! exactly backwards. A neutral module both layers can depend on keeps the
//! direction of the dependency honest.
//!
//! The trait itself exists so the encode and decode drivers can be written once
//! and reused over any slice of byte containers, and over a mutable shard slice
//! with one element split out for writing. It is monomorphized at every call
//! site, so it costs nothing at runtime.

/// Read-only access to a set of equal-length shards.
pub(crate) trait ShardSource {
    /// Number of shards available.
    fn count(&self) -> usize;
    /// Returns shard `index`.
    fn shard(&self, index: usize) -> &[u8];
}

/// Every slice of things that are byte slices.
///
/// One blanket impl rather than one per container is what lets a caller pass
/// whatever it already holds. `&[u8]`, `&mut [u8]`, `Vec<u8>`, `Box<[u8]>` and
/// `[u8; N]` all satisfy `AsRef<[u8]>`, so `[&[u8]]`, `[Vec<u8>]`, `[[u8; N]]`
/// and the rest are all shard sources with no adapter slice in between.
///
/// A signature fixed to `&[&[u8]]` instead makes every caller whose shards live
/// in a `Vec<Vec<u8>>` build a throwaway pointer slice on every call. For a
/// per-packet codec that is an allocation on the hot path spent satisfying a
/// type rather than doing any work, which is the cost this impl removes.
impl<E: AsRef<[u8]>> ShardSource for [E] {
    #[inline]
    fn count(&self) -> usize {
        self.len()
    }
    #[inline]
    fn shard(&self, index: usize) -> &[u8] {
        self[index].as_ref()
    }
}
