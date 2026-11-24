//! Copy-on-write inode map with precise mutation tracking.
//!
//! The in-memory representation is independent of the serialized Metadata
//! format: JSON always writes keys in increasing inode order.
use super::types::{Inode, InodeId};
use imbl::{HashMap, OrdSet};
use serde::de::Deserializer;
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Index;
use std::sync::{Arc, OnceLock};

/// Derived index of physical member references. It is deliberately not part
/// of the wire format or integrity digest; the inode table is authoritative.
/// A committed COW baseline makes incremental removal/replacement exact even
/// when get_mut callers change arbitrary blocks through an ordinary &mut Inode.
#[derive(Clone, Debug)]
struct MemberCatalog {
    base: HashMap<InodeId, Inode>,
    members: BTreeMap<String, OrdSet<InodeId>>,
}

fn inode_members(inode: Option<&Inode>) -> BTreeSet<&str> {
    inode
        .into_iter()
        .flat_map(|inode| &inode.blocks)
        .flat_map(|block| &block.shards)
        .map(|shard| shard.disk_id.as_str())
        .collect()
}

impl MemberCatalog {
    fn from_map(map: &HashMap<InodeId, Inode>) -> Self {
        let mut members = BTreeMap::<String, OrdSet<InodeId>>::new();
        for (ino, inode) in map {
            for disk_id in inode_members(Some(inode)) {
                members.entry(disk_id.to_owned()).or_default().insert(*ino);
            }
        }
        Self {
            base: map.clone(),
            members,
        }
    }

    fn advance(&mut self, map: &HashMap<InodeId, Inode>, dirty: &BTreeSet<InodeId>) {
        for ino in dirty {
            let previous = inode_members(self.base.get(ino));
            let current = inode_members(map.get(ino));
            for disk_id in previous.difference(&current) {
                if let Some(inodes) = self.members.get_mut(*disk_id) {
                    inodes.remove(ino);
                    if inodes.is_empty() {
                        self.members.remove(*disk_id);
                    }
                }
            }
            for disk_id in current.difference(&previous) {
                self.members
                    .entry((*disk_id).to_owned())
                    .or_default()
                    .insert(*ino);
            }
        }
        self.base = map.clone();
    }
}

#[derive(Clone, Debug)]
pub struct TrackedInodes {
    map: HashMap<InodeId, Inode>,
    dirty: BTreeSet<InodeId>,
    // Forked snapshots share their baseline; a commit/recovery resets it.
    // Distinct baselines require a full comparison, never a dirty-only diff.
    baseline: Arc<()>,
    member_catalog: OnceLock<MemberCatalog>,
}

impl Default for TrackedInodes {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            dirty: BTreeSet::new(),
            baseline: Arc::new(()),
            member_catalog: OnceLock::new(),
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
        if let Some(catalog) = self.member_catalog.get_mut() {
            catalog.advance(&self.map, &self.dirty);
        }
        self.dirty.clear();
        self.baseline = Arc::new(());
    }

    /// Build once on writable open. Deserializing temporary metadata for
    /// hashing and journal validation must not allocate a second catalog.
    pub(crate) fn initialize_member_catalog(&self) {
        self.member_catalog
            .get_or_init(|| MemberCatalog::from_map(&self.map));
    }

    /// Sorted inode candidates for a member, including uncommitted mutations.
    /// The committed index is shared across COW snapshots. Overlay only dirty
    /// inodes, so deletions, repairs, rollback and deferred writes cannot leave
    /// missing or stale candidates. Callers still revalidate actual stripes.
    pub(crate) fn member_inode_ids(&self, disk_id: &str) -> Vec<InodeId> {
        let catalog = self
            .member_catalog
            .get_or_init(|| MemberCatalog::from_map(&self.map));
        let committed = catalog
            .members
            .get(disk_id)
            .into_iter()
            .flat_map(|set| set.iter());
        if self.dirty.is_empty() {
            return committed.copied().collect();
        }
        let mut ids = committed
            .filter(|ino| !self.dirty.contains(ino))
            .copied()
            .collect::<BTreeSet<_>>();
        ids.extend(self.dirty.iter().copied().filter(|ino| {
            self.map.get(ino).is_some_and(|inode| {
                inode
                    .blocks
                    .iter()
                    .any(|block| block.shards.iter().any(|shard| shard.disk_id == disk_id))
            })
        }));
        ids.into_iter().collect()
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
            member_catalog: OnceLock::new(),
        })
    }
}
