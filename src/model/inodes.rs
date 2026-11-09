//! Copy-on-write inode map with precise mutation tracking.
//!
//! The in-memory representation is independent of the serialized Metadata
//! format: JSON always writes keys in increasing inode order.
use super::types::{Inode, InodeId};
use imbl::HashMap;
use serde::de::Deserializer;
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ops::Index;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct TrackedInodes {
    map: HashMap<InodeId, Inode>,
    dirty: BTreeSet<InodeId>,
    // Forked snapshots share their baseline; a commit/recovery resets it.
    // Distinct baselines require a full comparison, never a dirty-only diff.
    baseline: Arc<()>,
}

impl Default for TrackedInodes {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            dirty: BTreeSet::new(),
            baseline: Arc::new(()),
        }
    }
}

impl TrackedInodes {
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, ino: &InodeId) -> Option<&Inode> {
        self.map.get(ino)
    }

    pub fn contains_key(&self, ino: &InodeId) -> bool {
        self.map.contains_key(ino)
    }

    pub fn get_mut(&mut self, ino: &InodeId) -> Option<&mut Inode> {
        if !self.map.contains_key(ino) {
            return None;
        }
        self.dirty.insert(*ino);
        self.map.get_mut(ino)
    }

    pub fn insert(&mut self, ino: InodeId, inode: Inode) -> Option<Inode> {
        self.dirty.insert(ino);
        self.map.insert(ino, inode)
    }

    pub fn remove(&mut self, ino: &InodeId) -> Option<Inode> {
        let removed = self.map.remove(ino);
        if removed.is_some() {
            self.dirty.insert(*ino);
        }
        removed
    }

    pub fn keys(&self) -> impl Iterator<Item = &InodeId> {
        self.map.keys()
    }

    pub fn values(&self) -> impl Iterator<Item = &Inode> {
        self.map.values()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&InodeId, &Inode)> {
        self.map.iter()
    }

    /// Only safe after durability is known; also establishes a fresh baseline.
    pub fn mark_committed(&mut self) {
        self.dirty.clear();
        self.baseline = Arc::new(());
    }

    /// Fast candidates are valid only when snapshots descend from one baseline.
    /// Independent (e.g. recovered) metadata images require an exhaustive scan.
    pub fn changed_candidates<'a>(&'a self, other: &'a Self) -> BTreeSet<InodeId> {
        if Arc::ptr_eq(&self.baseline, &other.baseline) {
            self.dirty.union(&other.dirty).copied().collect()
        } else {
            self.keys().chain(other.keys()).copied().collect()
        }
    }

    #[cfg(test)]
    pub fn pending_changes(&self) -> usize {
        self.dirty.len()
    }
}

impl Index<&InodeId> for TrackedInodes {
    type Output = Inode;
    fn index(&self, ino: &InodeId) -> &Self::Output {
        &self.map[ino]
    }
}

impl Extend<(InodeId, Inode)> for TrackedInodes {
    fn extend<T: IntoIterator<Item = (InodeId, Inode)>>(&mut self, items: T) {
        // Extending a live map is a mutation, not an implicit commit.
        for (ino, inode) in items {
            self.insert(ino, inode);
        }
    }
}

impl FromIterator<(InodeId, Inode)> for TrackedInodes {
    fn from_iter<T: IntoIterator<Item = (InodeId, Inode)>>(items: T) -> Self {
        let mut inodes = Self::default();
        inodes.extend(items);
        inodes.mark_committed();
        inodes
    }
}

impl<'a> IntoIterator for &'a TrackedInodes {
    type Item = (&'a InodeId, &'a Inode);
    type IntoIter = imbl::hashmap::Iter<'a, InodeId, Inode, imbl::shared_ptr::DefaultSharedPtr>;
    fn into_iter(self) -> Self::IntoIter {
        self.map.iter()
    }
}

impl IntoIterator for TrackedInodes {
    type Item = (InodeId, Inode);
    type IntoIter =
        imbl::hashmap::ConsumingIter<(InodeId, Inode), imbl::shared_ptr::DefaultSharedPtr>;
    fn into_iter(self) -> Self::IntoIter {
        self.map.into_iter()
    }
}

impl Serialize for TrackedInodes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sorted = self.map.iter().collect::<Vec<_>>();
        sorted.sort_unstable_by_key(|(ino, _)| *ino);
        let mut map = serializer.serialize_map(Some(sorted.len()))?;
        for (ino, inode) in sorted {
            map.serialize_entry(ino, inode)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for TrackedInodes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = HashMap::<InodeId, Inode>::deserialize(deserializer)?;
        Ok(Self {
            map,
            dirty: BTreeSet::new(),
            baseline: Arc::new(()),
        })
    }
}
