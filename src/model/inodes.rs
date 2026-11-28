//! Copy-on-write inode map with precise mutation tracking.
//!
//! The in-memory representation is independent of the serialized Metadata
//! format: JSON always writes keys in increasing inode order.
use super::types::{Inode, InodeId, NodeKind};
use imbl::{HashMap, OrdMap, OrdSet};
use serde::de::Deserializer;
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Bound, Index};
use std::sync::{Arc, OnceLock};

use super::checkpoint_chunks::{CheckpointChunks, CheckpointFrame};

/// A checkpoint-only derived extension. The normal Metadata serializer
/// deliberately omits it, so legacy integrity identities remain unchanged.
type MemberStripes = BTreeMap<String, OrdMap<InodeId, OrdSet<String>>>;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CheckpointMemberCatalog {
    version: u32,
    members: MemberStripes,
}

#[derive(Clone, Debug)]
struct MemberCatalog {
    base: HashMap<InodeId, Inode>,
    members: MemberStripes,
}

fn inode_references(inode: Option<&Inode>) -> BTreeMap<&str, BTreeSet<&str>> {
    let mut references = BTreeMap::<&str, BTreeSet<&str>>::new();
    for block in inode.into_iter().flat_map(|inode| &inode.blocks) {
        for shard in &block.shards {
            references
                .entry(&shard.disk_id)
                .or_default()
                .insert(&block.stripe_id);
        }
    }
    references
}

fn own_stripes(stripes: &BTreeSet<&str>) -> OrdSet<String> {
    stripes.iter().map(|stripe| (*stripe).to_owned()).collect()
}

impl MemberCatalog {
    fn from_map(map: &HashMap<InodeId, Inode>) -> Self {
        let mut members = MemberStripes::new();
        for (ino, inode) in map {
            for (disk_id, stripes) in inode_references(Some(inode)) {
                members
                    .entry(disk_id.to_owned())
                    .or_default()
                    .insert(*ino, own_stripes(&stripes));
            }
        }
        Self {
            base: map.clone(),
            members,
        }
    }

    fn advance(&mut self, map: &HashMap<InodeId, Inode>, dirty: &BTreeSet<InodeId>) {
        for ino in dirty {
            let previous = inode_references(self.base.get(ino));
            let current = inode_references(map.get(ino));
            for disk_id in previous.keys().filter(|disk| !current.contains_key(*disk)) {
                if let Some(inodes) = self.members.get_mut(*disk_id) {
                    inodes.remove(ino);
                    if inodes.is_empty() {
                        self.members.remove(*disk_id);
                    }
                }
            }
            for (disk_id, stripes) in &current {
                if previous.get(disk_id) != Some(stripes) {
                    self.members
                        .entry((*disk_id).to_owned())
                        .or_default()
                        .insert(*ino, own_stripes(stripes));
                }
            }
        }
        self.base = map.clone();
    }
}

// Check the derived catalog against the authoritative inode stream while it
// is decoded, avoiding a second full walk of the loaded namespace. Both sides
// hash ordered, length-delimited (inode, stripe) references per member.
type ReferenceProof = BTreeMap<String, [u8; 32]>;
#[derive(Default)]
struct ProofBuilder {
    digests: BTreeMap<String, Sha256>,
    last: Option<InodeId>,
    unordered: bool,
}
fn hash_references<'a>(
    hash: &mut Sha256,
    ino: InodeId,
    count: usize,
    stripes: impl Iterator<Item = &'a str>,
) {
    hash.update(ino.to_le_bytes());
    hash.update((count as u64).to_le_bytes());
    for stripe in stripes {
        hash.update((stripe.len() as u64).to_le_bytes());
        hash.update(stripe.as_bytes());
    }
}
impl ProofBuilder {
    fn observe(&mut self, ino: InodeId, inode: &Inode) {
        if self.last.is_some_and(|last| last >= ino) {
            self.unordered = true;
        }
        self.last = Some(ino);
        for (disk, stripes) in inode_references(Some(inode)) {
            hash_references(
                self.digests.entry(disk.to_owned()).or_default(),
                ino,
                stripes.len(),
                stripes.into_iter(),
            );
        }
    }
    fn finish(self) -> Option<ReferenceProof> {
        (!self.unordered).then(|| {
            self.digests
                .into_iter()
                .map(|(disk, hash)| (disk, hash.finalize().into()))
                .collect()
        })
    }
}
fn catalog_proof(members: &MemberStripes) -> Option<ReferenceProof> {
    let mut proof = ReferenceProof::new();
    for (member, inodes) in members {
        if inodes.is_empty() {
            return None;
        }
        let mut hash = Sha256::new();
        for (ino, stripes) in inodes {
            if stripes.is_empty() {
                return None;
            }
            hash_references(
                &mut hash,
                *ino,
                stripes.len(),
                stripes.iter().map(String::as_str),
            );
        }
        proof.insert(member.clone(), hash.finalize().into());
    }
    Some(proof)
}

thread_local! { static CAPTURE_CHECKPOINT_PROOF: Cell<bool> = const { Cell::new(false) }; }
pub(crate) struct CheckpointProofGuard(bool);
impl Drop for CheckpointProofGuard {
    fn drop(&mut self) {
        CAPTURE_CHECKPOINT_PROOF.with(|flag| flag.set(self.0));
    }
}
pub(crate) fn capture_checkpoint_proof(enabled: bool) -> CheckpointProofGuard {
    CheckpointProofGuard(CAPTURE_CHECKPOINT_PROOF.with(|flag| flag.replace(enabled)))
}

#[derive(Clone, Debug)]
pub struct TrackedInodes {
    checkpoint_chunks: OnceLock<CheckpointChunks>,
    map: HashMap<InodeId, Inode>,
    dirty: BTreeSet<InodeId>,
    // Forked snapshots share their baseline; a commit/recovery resets it.
    // Distinct baselines require a full comparison, never a dirty-only diff.
    baseline: Arc<()>,
    member_catalog: OnceLock<MemberCatalog>,
    // Derived, lazily populated ordered file IDs. Snapshot clones share the
    // persistent tree; dirty inode IDs reconcile arbitrary edits and rollback.
    file_ids: OnceLock<OrdSet<InodeId>>,
    checkpoint_proof: Option<ReferenceProof>,
    // Only set on the temporary serialization view for legacy integrity.
    normalize_legacy_hash: bool,
}

impl Default for TrackedInodes {
    fn default() -> Self {
        Self {
            checkpoint_chunks: OnceLock::new(),
            map: HashMap::new(),
            dirty: BTreeSet::new(),
            baseline: Arc::new(()),
            member_catalog: OnceLock::new(),
            file_ids: OnceLock::new(),
            checkpoint_proof: None,
            normalize_legacy_hash: false,
        }
    }
}

impl TrackedInodes {
    /// Preserve the historical JSON round-trip of each inode without building
    /// a second namespace. This view is used only by the legacy hash writer.
    pub(crate) fn into_legacy_hash_view(mut self) -> Self {
        self.normalize_legacy_hash = true;
        self
    }

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
        if let Some(chunks) = self.checkpoint_chunks.get_mut() {
            chunks.invalidate(*ino);
        }
        self.map.get_mut(ino)
    }

    pub fn insert(&mut self, ino: InodeId, inode: Inode) -> Option<Inode> {
        self.dirty.insert(ino);
        if let Some(chunks) = self.checkpoint_chunks.get_mut() {
            chunks.insert(ino);
        }
        self.map.insert(ino, inode)
    }

    pub fn remove(&mut self, ino: &InodeId) -> Option<Inode> {
        let removed = self.map.remove(ino);
        if removed.is_some() {
            self.dirty.insert(*ino);
            if let Some(chunks) = self.checkpoint_chunks.get_mut() {
                chunks.remove(*ino);
            }
        }
        removed
    }

    pub(crate) fn checkpoint_frame_count(&self) -> usize {
        self.checkpoint_chunks
            .get_or_init(|| CheckpointChunks::from_map(&self.map))
            .len()
    }

    pub(crate) fn checkpoint_frames(&self) -> crate::error::Result<Vec<Arc<CheckpointFrame>>> {
        self.checkpoint_chunks
            .get_or_init(|| CheckpointChunks::from_map(&self.map))
            .frames(&self.map)
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
        if let Some(ids) = self.file_ids.get_mut() {
            Self::refresh_file_ids(ids, &self.map, &self.dirty);
        }
        self.dirty.clear();
        self.baseline = Arc::new(());
    }

    fn refresh_file_ids(
        ids: &mut OrdSet<InodeId>,
        map: &HashMap<InodeId, Inode>,
        dirty: &BTreeSet<InodeId>,
    ) {
        for ino in dirty {
            let is_file = map
                .get(ino)
                .is_some_and(|inode| inode.kind == NodeKind::File);
            match (is_file, ids.contains(ino)) {
                (true, false) => {
                    ids.insert(*ino);
                }
                (false, true) => {
                    ids.remove(ino);
                }
                _ => {}
            }
        }
    }

    /// A bounded numeric cursor window over live file IDs. The first call
    /// builds the index; subsequent calls only reconcile dirty inodes and
    /// seek directly to the cursor. No file data or shard records are cloned.
    pub(crate) fn file_window_ids(
        &self,
        cursor: Option<InodeId>,
        max_files: usize,
    ) -> Vec<InodeId> {
        if max_files == 0 {
            return Vec::new();
        }
        let mut ids = self
            .file_ids
            .get_or_init(|| {
                self.map
                    .iter()
                    .filter_map(|(ino, inode)| (inode.kind == NodeKind::File).then_some(*ino))
                    .collect()
            })
            .clone();
        Self::refresh_file_ids(&mut ids, &self.map, &self.dirty);
        match cursor {
            Some(cursor) => ids
                .range::<_, InodeId>((Bound::Excluded(cursor), Bound::Unbounded))
                .chain(ids.range(..=cursor))
                .take(max_files)
                .copied()
                .collect(),
            None => ids.iter().take(max_files).copied().collect(),
        }
    }

    /// Build once on writable open. Deserializing temporary metadata for
    /// hashing and journal validation must not allocate a second catalog.
    pub(crate) fn initialize_member_catalog(&self) {
        self.member_catalog
            .get_or_init(|| MemberCatalog::from_map(&self.map));
    }

    #[cfg(test)]
    pub(crate) fn has_member_catalog(&self) -> bool {
        self.member_catalog.get().is_some()
    }

    /// Encode the exact current reference index, including uncommitted edits.
    pub(crate) fn checkpoint_catalog(&self) -> CheckpointMemberCatalog {
        let mut catalog = self
            .member_catalog
            .get_or_init(|| MemberCatalog::from_map(&self.map))
            .clone();
        catalog.advance(&self.map, &self.dirty);
        CheckpointMemberCatalog {
            version: 1,
            members: catalog.members,
        }
    }

    /// The payload checksums protect the bytes; this proof additionally
    /// prevents a validly checksummed but stale/incomplete derived index from
    /// hiding a live shard. Legacy or malformed hints take the normal path.
    pub(crate) fn restore_checkpoint_catalog(
        &mut self,
        catalog: Option<CheckpointMemberCatalog>,
    ) -> bool {
        let proof = self.checkpoint_proof.take();
        let Some(catalog) = catalog else {
            return false;
        };
        if catalog.version != 1 || proof.is_none() || catalog_proof(&catalog.members) != proof {
            return false;
        }
        self.member_catalog = OnceLock::from(MemberCatalog {
            base: self.map.clone(),
            members: catalog.members,
        });
        true
    }

    /// Sorted member worklist, including modifications since the last commit.
    pub(crate) fn member_stripes(&self, disk_id: &str) -> Vec<(InodeId, BTreeSet<String>)> {
        let catalog = self
            .member_catalog
            .get_or_init(|| MemberCatalog::from_map(&self.map));
        let mut refs = catalog
            .members
            .get(disk_id)
            .into_iter()
            .flat_map(|inodes| inodes.iter())
            .filter(|(ino, _)| !self.dirty.contains(ino))
            .map(|(ino, stripes)| (*ino, stripes.iter().cloned().collect::<BTreeSet<_>>()))
            .collect::<BTreeMap<_, _>>();
        for ino in &self.dirty {
            if let Some(stripes) = inode_references(self.map.get(ino)).get(disk_id) {
                refs.insert(*ino, stripes.iter().map(|id| (*id).to_owned()).collect());
            }
        }
        refs.into_iter().collect()
    }

    pub(crate) fn member_inode_ids(&self, disk_id: &str) -> Vec<InodeId> {
        let catalog = self
            .member_catalog
            .get_or_init(|| MemberCatalog::from_map(&self.map));
        let committed = catalog
            .members
            .get(disk_id)
            .into_iter()
            .flat_map(|inodes| inodes.keys());
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
        if self.normalize_legacy_hash {
            let mut compact = Vec::new();
            for (ino, inode) in sorted {
                compact.clear();
                serde_json::to_writer(&mut compact, inode).map_err(serde::ser::Error::custom)?;
                let normalized: Inode =
                    serde_json::from_slice(&compact).map_err(serde::ser::Error::custom)?;
                map.serialize_entry(ino, &normalized)?;
            }
        } else {
            for (ino, inode) in sorted {
                map.serialize_entry(ino, inode)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for TrackedInodes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if !CAPTURE_CHECKPOINT_PROOF.with(Cell::get) {
            let map = HashMap::<InodeId, Inode>::deserialize(deserializer)?;
            return Ok(Self {
                map,
                ..Self::default()
            });
        }
        struct InodesVisitor;
        impl<'de> serde::de::Visitor<'de> for InodesVisitor {
            type Value = TrackedInodes;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("inode map")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut entries: A,
            ) -> Result<Self::Value, A::Error> {
                let mut map = HashMap::new();
                let mut proof = ProofBuilder::default();
                while let Some((ino, inode)) = entries.next_entry::<InodeId, Inode>()? {
                    proof.observe(ino, &inode);
                    map.insert(ino, inode);
                }
                Ok(TrackedInodes {
                    map,
                    checkpoint_proof: proof.finish(),
                    ..TrackedInodes::default()
                })
            }
        }
        deserializer.deserialize_map(InodesVisitor)
    }
}
