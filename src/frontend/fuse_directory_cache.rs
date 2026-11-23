//! Bounded, mutation-aware FUSE directory enumeration snapshots.
//!
//! Kernel readdir offsets are *ordinal* cookies. Rebuilding and re-sorting a
//! large directory on every FUSE buffer request is quadratic in its size.
//! Cache only its immutable listing across request boundaries; do not cache
//! readdirplus attributes, which may change without the parent directory
//! changing.

use crate::types::InodeId;
use crate::volume::{DirEntry, NodeAttr};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(super) const MAX_DIRECTORY_LISTING_CACHE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DirectoryRevision {
    mtime: u64,
    ctime: u64,
    nlink: u32,
    size: u64,
}

impl From<&NodeAttr> for DirectoryRevision {
    fn from(attr: &NodeAttr) -> Self {
        Self {
            mtime: attr.mtime.to_bits(),
            ctime: attr.ctime.to_bits(),
            nlink: attr.nlink,
            size: attr.size,
        }
    }
}

struct CachedDirectory {
    revision: DirectoryRevision,
    entries: Arc<Vec<DirEntry>>,
    bytes: usize,
    used: u64,
}

pub(super) struct DirectoryListingCache {
    max_bytes: usize,
    entries: BTreeMap<InodeId, CachedDirectory>,
    used_bytes: usize,
    tick: u64,
}

impl Default for DirectoryListingCache {
    fn default() -> Self {
        Self {
            max_bytes: MAX_DIRECTORY_LISTING_CACHE_BYTES,
            entries: BTreeMap::new(),
            used_bytes: 0,
            tick: 0,
        }
    }
}

impl DirectoryListingCache {
    pub(super) fn get(
        &mut self,
        inode: InodeId,
        revision: DirectoryRevision,
    ) -> Option<Arc<Vec<DirEntry>>> {
        if self
            .entries
            .get(&inode)
            .is_some_and(|cached| cached.revision != revision)
        {
            self.remove(inode);
            return None;
        }
        self.tick = self.tick.wrapping_add(1);
        let cached = self.entries.get_mut(&inode)?;
        cached.used = self.tick;
        Some(Arc::clone(&cached.entries))
    }

    pub(super) fn insert(
        &mut self,
        inode: InodeId,
        revision: DirectoryRevision,
        entries: Arc<Vec<DirEntry>>,
    ) {
        // BTreeMap lookups are cheap; store only bounded snapshots. Arc clones
        // used by in-flight replies remain alive until the reply completes.
        let bytes = std::mem::size_of::<DirEntry>()
            .saturating_mul(entries.capacity())
            .saturating_add(
                entries
                    .iter()
                    .map(|entry| {
                        entry
                            .name
                            .capacity()
                            .saturating_add(entry.name_bytes.capacity())
                    })
                    .fold(0usize, usize::saturating_add),
            );
        self.remove(inode);
        if bytes > self.max_bytes {
            return;
        }
        while self.used_bytes.saturating_add(bytes) > self.max_bytes {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, cached)| cached.used)
                .map(|(ino, _)| *ino)
            else {
                break;
            };
            self.remove(oldest);
        }
        self.tick = self.tick.wrapping_add(1);
        self.entries.insert(
            inode,
            CachedDirectory {
                revision,
                entries,
                bytes,
                used: self.tick,
            },
        );
        self.used_bytes += bytes;
    }

    fn remove(&mut self, inode: InodeId) {
        if let Some(old) = self.entries.remove(&inode) {
            self.used_bytes -= old.bytes;
        }
    }

    #[cfg(test)]
    pub(super) fn used_bytes(&self) -> usize {
        self.used_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeKind;

    fn sample_attr(ino: InodeId) -> NodeAttr {
        NodeAttr {
            ino,
            kind: NodeKind::Directory,
            mode: 0o755,
            uid: 0,
            gid: 0,
            nlink: 2,
            size: 4096,
            rdev: 0,
            atime: 1.0,
            mtime: 2.0,
            ctime: 3.0,
            blocks: 0,
            blksize: 4096,
        }
    }

    fn listing(ino: InodeId) -> Arc<Vec<DirEntry>> {
        Arc::new(vec![
            DirEntry {
                name: ".".into(),
                name_bytes: b".".to_vec(),
                attr: sample_attr(ino),
            },
            DirEntry {
                name: "test".into(),
                name_bytes: b"test".to_vec(),
                attr: sample_attr(ino + 1),
            },
        ])
    }

    #[test]
    fn directory_cache_preserves_snapshot_until_parent_revision_changes() {
        let mut cache = DirectoryListingCache::default();
        let revision = DirectoryRevision::from(&sample_attr(1));
        let first = listing(1);
        cache.insert(1, revision, Arc::clone(&first));
        assert!(cache.used_bytes() > 0);
        assert!(Arc::ptr_eq(&cache.get(1, revision).unwrap(), &first));
        let mut changed = sample_attr(1);
        changed.mtime = 4.0;
        assert!(cache.get(1, DirectoryRevision::from(&changed)).is_none());
        assert_eq!(cache.used_bytes(), 0);
    }

    #[test]
    fn lru_eviction_caps_cache_without_invalidating_in_flight_snapshots() {
        let mut cache = DirectoryListingCache::default();
        let first = listing(11);
        let rev = DirectoryRevision::from(&first[0].attr);
        cache.insert(11, rev, Arc::clone(&first));
        let per_listing = cache.used_bytes();
        assert!(per_listing > 0);
        cache.max_bytes = per_listing;
        // Inserting another listing evicts the first, but any in-flight
        // Arc continues to refer to its valid snapshot through the reply.
        let second = listing(21);
        cache.insert(
            21,
            DirectoryRevision::from(&second[0].attr),
            Arc::clone(&second),
        );
        assert!(cache.used_bytes() <= per_listing);
        assert!(cache.get(11, rev).is_none());
        assert!(Arc::ptr_eq(
            &cache
                .get(21, DirectoryRevision::from(&second[0].attr))
                .unwrap(),
            &second
        ));
        assert_eq!(first.len(), 2);
        cache.max_bytes = per_listing - 1;
        cache.insert(31, DirectoryRevision::from(&sample_attr(31)), listing(31));
        assert!(cache
            .get(31, DirectoryRevision::from(&sample_attr(31)))
            .is_none());
    }
}
