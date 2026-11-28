//! Experimental V3 logical schema. Keys are directly ordered and values have
//! explicit version/codec framing. No sequential record-ID index is needed.
//! This is not yet a supported pool format: volume integration and topology/
//! migration qualification must precede any V3 mkfs/upgrade.
use super::{
    CertifiedReader, ScanOrder, ScanPage, ScanWindow, TypedChanges, TypedKey, TypedMetadata,
};
use crate::error::{ArgosError, Result};
use crate::metadata::store::InodeCorePage;
use crate::types::{FileBlock, Metadata, RawAllocatorState};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

/// Stable logical domains; unknown domains are not interpreted by this layer.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Domain {
    Header = 1,
    Inode = 2,
    Dirent = 3,
    Xattr = 4,
    Block = 5,
    Parent = 6,
    Disk = 7,
    DataAllocator = 8,
    DataFree = 9,
    FileIndex = 10,
    Quota = 11,
    MemberReference = 12,
    Event = 13,
    Snapshot = 14,
    QuotaMembership = 15,
    Statistics = 16,
}

impl Domain {
    pub fn prefix(self) -> TypedKey {
        TypedKey::new(self as u16, &[]).expect("constant domain")
    }
    pub fn inode_prefix(self, ino: u64) -> TypedKey {
        TypedKey::new(self as u16, &ino.to_be_bytes()).expect("fixed inode key")
    }
    pub fn inode_name(self, ino: u64, name: &[u8]) -> Result<TypedKey> {
        let mut bytes = ino.to_be_bytes().to_vec();
        bytes.extend_from_slice(name);
        TypedKey::new(self as u16, &bytes)
    }
    pub fn inode_offset(self, ino: u64, offset: u64) -> TypedKey {
        let mut bytes = ino.to_be_bytes().to_vec();
        bytes.extend_from_slice(&offset.to_be_bytes());
        TypedKey::new(self as u16, &bytes).expect("fixed extent key")
    }
    pub fn parent(child: u64, parent: u64, name: &[u8]) -> Result<TypedKey> {
        let mut bytes = child.to_be_bytes().to_vec();
        bytes.extend_from_slice(&parent.to_be_bytes());
        bytes.extend_from_slice(name);
        TypedKey::new(Self::Parent as u16, &bytes)
    }
    pub fn member_block(id: &str, ino: u64, offset: u64) -> Result<TypedKey> {
        let prefix = Self::MemberReference.disk(id, None)?;
        let mut bytes = prefix.as_bytes().to_vec();
        bytes.extend_from_slice(&ino.to_be_bytes());
        bytes.extend_from_slice(&offset.to_be_bytes());
        TypedKey::from_bytes(bytes)
    }

    /// Length-prefix device IDs so variable-length IDs cannot collide with
    /// their offset suffix. Ordering within an individual device is numeric.
    pub fn disk(self, id: &str, offset: Option<u64>) -> Result<TypedKey> {
        let size = u16::try_from(id.len())
            .map_err(|_| ArgosError::Invalid("metadata device ID too long".into()))?;
        let mut bytes = size.to_be_bytes().to_vec();
        bytes.extend_from_slice(id.as_bytes());
        if let Some(offset) = offset {
            bytes.extend_from_slice(&offset.to_be_bytes());
        }
        TypedKey::new(self as u16, &bytes)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct QuotaRecord {
    pub limit: u64,
    pub used: u128,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct VolumeStatistics {
    pub inodes: u64,
    pub files: u64,
    pub logical_bytes: u128,
}

fn member_block_keys(ino: u64, block: &FileBlock) -> Result<BTreeSet<TypedKey>> {
    block
        .shards
        .iter()
        .map(|shard| Domain::member_block(&shard.disk_id, ino, block.raw_offset))
        .collect()
}

const MAGIC: &[u8; 4] = b"ARV3";
const VERSION: u16 = 1;
const MAX_RECORD: usize = 128 * 1024 * 1024;
const HEADER: usize = 12;

/// A record envelope separates the schema version from page format. Unknown
/// versions/codecs fail closed. Length is authenticated by the containing tree.
pub fn encode_record(value: &impl Serialize) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(value)?;
    if json.len() > MAX_RECORD {
        return Err(ArgosError::FileTooLarge("V3 metadata record".into()));
    }
    let size = json.len();
    let compressed = lz4_flex::compress(&json);
    let (codec, payload) = if compressed.len() < json.len() {
        (1u16, compressed)
    } else {
        (0u16, json)
    };
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&codec.to_le_bytes());
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn decode_record<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let corrupt = || ArgosError::CorruptedMetadata("invalid V3 metadata record envelope".into());
    if bytes.len() < HEADER || &bytes[..4] != MAGIC {
        return Err(corrupt());
    }
    let version = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
    if version != VERSION {
        return Err(ArgosError::IncompatibleFormat(format!(
            "V3 record version {version}"
        )));
    }
    let codec = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
    let size = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let payload = &bytes[HEADER..];
    if size > MAX_RECORD || payload.len() > MAX_RECORD {
        return Err(corrupt());
    }
    match codec {
        0 if payload.len() == size => Ok(serde_json::from_slice(payload)?),
        1 if payload.len() < size => {
            let decoded = lz4_flex::decompress(payload, size).map_err(|_| corrupt())?;
            if decoded.len() != size {
                return Err(corrupt());
            }
            Ok(serde_json::from_slice(&decoded)?)
        }
        0 | 1 => Err(corrupt()),
        _ => Err(ArgosError::IncompatibleFormat(format!(
            "V3 record codec {codec}"
        ))),
    }
}

fn put(changes: &mut TypedChanges, key: TypedKey, value: &impl Serialize) -> Result<()> {
    if changes.insert(key, Some(encode_record(value)?)).is_some() {
        return Err(ArgosError::CorruptedMetadata(
            "duplicate V3 logical key".into(),
        ));
    }
    Ok(())
}

/// Convert the authenticated legacy in-memory image into a logical V3
/// record set. This is for offline conversion tests, not an in-place upgrade.
/// Decode legacy escaped names to lossless binary compound keys.
pub fn import_image(meta: &Metadata) -> Result<TypedChanges> {
    import_image_with_events(meta, &[])
}

/// Convert an authenticated offline image and its available audit events.
/// The source must remain immutable for the entire import.
pub fn import_image_with_events(
    meta: &Metadata,
    events: &[crate::types::PagedAuditEvent],
) -> Result<TypedChanges> {
    let mut changes = TypedChanges::new();
    let mut stats = VolumeStatistics::default();
    let mut header = meta.clone();
    header.inodes = Default::default();
    header.disks.clear();
    header.raw_pool.allocators.clear();
    header.integrity.meta_hash.clear();
    header.checkpoint_member_catalog = None;
    put(&mut changes, Domain::Header.prefix(), &header)?;
    for (id, disk) in &meta.disks {
        put(&mut changes, Domain::Disk.disk(id, None)?, disk)?;
    }
    for (id, allocator) in &meta.raw_pool.allocators {
        let mut core: RawAllocatorState = allocator.clone();
        core.free_extents = Default::default();
        put(&mut changes, Domain::DataAllocator.disk(id, None)?, &core)?;
        for extent in allocator.free_extents.iter() {
            put(
                &mut changes,
                Domain::DataFree.disk(id, Some(extent.offset))?,
                extent,
            )?;
        }
    }
    for (&ino, inode) in meta.inodes.iter() {
        if ino == 0 || ino != inode.id {
            return Err(ArgosError::CorruptedMetadata(
                "invalid source inode identity".into(),
            ));
        }
        stats.inodes += 1;
        put(
            &mut changes,
            Domain::Inode.inode_prefix(ino),
            &InodeCorePage::from(inode),
        )?;
        if inode.kind == crate::types::NodeKind::File {
            stats.files += 1;
            stats.logical_bytes += u128::from(inode.size);
            put(&mut changes, Domain::FileIndex.inode_prefix(ino), &())?;
        }
        for (name, &child) in &inode.entries {
            let name = crate::volume::decode_entry_name_bytes(name);
            put(&mut changes, Domain::Dirent.inode_name(ino, &name)?, &child)?;
            put(&mut changes, Domain::parent(child, ino, &name)?, &())?;
        }
        for (name, value) in &inode.xattrs {
            put(
                &mut changes,
                Domain::Xattr.inode_name(ino, name.as_bytes())?,
                value,
            )?;
        }
        for block in &inode.blocks {
            put(
                &mut changes,
                Domain::Block.inode_offset(ino, block.raw_offset),
                block,
            )?;
            for key in member_block_keys(ino, block)? {
                put(&mut changes, key, &block.stripe_id)?;
            }
        }
    }
    put(&mut changes, Domain::Statistics.prefix(), &stats)?;
    // Reuse the supported format's quota accounting, including hard-link
    // deduplication and nested quota memberships, rather than inventing a
    // second interpretation during conversion.
    let quotas = crate::volume::QuotaRuntime::from_metadata(meta)?;
    for (&ino, &used) in &quotas.usage_bytes {
        let limit = meta
            .inodes
            .get(&ino)
            .and_then(|inode| inode.quota_bytes)
            .ok_or_else(|| ArgosError::CorruptedMetadata("quota index missing limit".into()))?;
        put(
            &mut changes,
            Domain::Quota.inode_prefix(ino),
            &QuotaRecord { limit, used },
        )?;
    }
    for (&ino, scopes) in &quotas.memberships {
        for (&quota, &count) in scopes {
            if count == 0 {
                return Err(ArgosError::CorruptedMetadata(
                    "zero quota membership".into(),
                ));
            }
            put(
                &mut changes,
                Domain::QuotaMembership.inode_offset(ino, quota),
                &count,
            )?;
        }
    }
    for event in events {
        if event.txid > meta.txid {
            return Err(ArgosError::CorruptedMetadata(
                "future audit event in V3 import".into(),
            ));
        }
        put(
            &mut changes,
            Domain::Event.inode_prefix(event.sequence),
            event,
        )?;
    }
    Ok(changes)
}

/// Logical transaction updates can combine arbitrary domains into one COW
/// root publication. Typed helpers avoid accidental numeric record IDs.
pub struct RecordTransaction {
    expected: (u64, [u8; 32]),
    namespace: [u8; 32],
    changes: TypedChanges,
    dirent_preconditions: BTreeMap<TypedKey, Option<u64>>,
    block_preconditions: BTreeMap<TypedKey, Option<FileBlock>>,
}

impl RecordTransaction {
    pub fn begin(tree: &TypedMetadata) -> Self {
        Self {
            expected: (tree.txid(), tree.root_hash()),
            namespace: tree.namespace,
            changes: TypedChanges::new(),
            dirent_preconditions: BTreeMap::new(),
            block_preconditions: BTreeMap::new(),
        }
    }
    pub fn put(&mut self, key: TypedKey, value: &impl Serialize) -> Result<()> {
        self.changes.insert(key, Some(encode_record(value)?));
        Ok(())
    }
    pub fn delete(&mut self, key: TypedKey) {
        self.changes.insert(key, None);
    }
    fn check_generation(&self, tree: &TypedMetadata) -> Result<()> {
        if (tree.txid(), tree.root_hash()) != self.expected || tree.namespace != self.namespace {
            return Err(ArgosError::Conflict("stale V3 metadata transaction".into()));
        }
        Ok(())
    }

    /// Point lookup in the dirty overlay, then the pinned generation.
    pub fn get_record(&self, tree: &TypedMetadata, key: TypedKey) -> Result<Option<Vec<u8>>> {
        self.check_generation(tree)?;
        match self.changes.get(&key) {
            Some(value) => Ok(value.clone()),
            None => tree.get(key),
        }
    }

    /// Stage an existing regular file's core and its logical usage counters.
    /// Data-plane callers must stage any corresponding block and physical
    /// extent updates in this same transaction. This is not a truncate API.
    ///
    /// Changing inode identity, kind, link count or quota membership requires
    /// the namespace transaction path, not this accounting helper.
    pub fn update_file_core(&mut self, tree: &TypedMetadata, new: &InodeCorePage) -> Result<()> {
        self.check_generation(tree)?;
        let key = Domain::Inode.inode_prefix(new.id);
        let old: InodeCorePage = self
            .get_record(tree, key.clone())?
            .as_deref()
            .map(decode_record)
            .transpose()?
            .ok_or_else(|| ArgosError::NotFound(format!("inode {}", new.id)))?;
        if old.id != new.id
            || old.kind != crate::types::NodeKind::File
            || new.kind != old.kind
            || old.nlink != new.nlink
            || new.quota_bytes != old.quota_bytes
        {
            return Err(ArgosError::Invalid(
                "V3 file core update changes namespace identity".into(),
            ));
        }
        let mut staged = TypedChanges::new();
        put(&mut staged, key, new)?;
        if old.size != new.size {
            let delta = |used: u128| -> Result<u128> {
                let value = used
                    .checked_sub(u128::from(old.size))
                    .and_then(|used| used.checked_add(u128::from(new.size)))
                    .ok_or_else(|| {
                        ArgosError::CorruptedMetadata(
                            "V3 logical usage counter overflow or underflow".into(),
                        )
                    })?;
                Ok(value)
            };
            let stats_key = Domain::Statistics.prefix();
            let mut stats: VolumeStatistics = self
                .get_record(tree, stats_key.clone())?
                .as_deref()
                .map(decode_record)
                .transpose()?
                .ok_or_else(|| {
                    ArgosError::CorruptedMetadata("missing V3 volume statistics".into())
                })?;
            stats.logical_bytes = delta(stats.logical_bytes)?;
            put(&mut staged, stats_key, &stats)?;
            let prefix = Domain::QuotaMembership.inode_prefix(new.id);
            let end = prefix
                .prefix_end()
                .map_or(Bound::Unbounded, Bound::Excluded);
            // Until the namespace writer is connected, do not mix changes
            // to memberships with this helper: the current on-disk set
            // must remain the authoritative list of affected quotas.
            if self
                .changes
                .range((Bound::Included(prefix.clone()), end.clone()))
                .next()
                .is_some()
            {
                return Err(ArgosError::Conflict(
                    "V3 quota membership changed in transaction".into(),
                ));
            }
            let mut window = ScanWindow {
                start: Bound::Included(prefix.clone()),
                end,
                max_records: 128,
                max_bytes: 64 * 1024,
                order: ScanOrder::Ascending,
            };
            loop {
                let page = tree.scan(&window)?;
                for (member, value) in &page.records {
                    let suffix = member
                        .as_bytes()
                        .strip_prefix(prefix.as_bytes())
                        .and_then(|suffix| <[u8; 8]>::try_from(suffix).ok())
                        .ok_or_else(|| {
                            ArgosError::CorruptedMetadata("invalid V3 quota membership key".into())
                        })?;
                    let quota = u64::from_be_bytes(suffix);
                    if decode_record::<u64>(value)? == 0 {
                        return Err(ArgosError::CorruptedMetadata(
                            "zero V3 quota membership".into(),
                        ));
                    }
                    let quota_key = Domain::Quota.inode_prefix(quota);
                    let mut record: QuotaRecord = self
                        .get_record(tree, quota_key.clone())?
                        .as_deref()
                        .map(decode_record)
                        .transpose()?
                        .ok_or_else(|| {
                            ArgosError::CorruptedMetadata("missing V3 quota record".into())
                        })?;
                    record.used = delta(record.used)?;
                    if new.size > old.size && record.used > u128::from(record.limit) {
                        return Err(ArgosError::QuotaExceeded {
                            inode: quota,
                            used: record.used.min(u128::from(u64::MAX)) as u64,
                            limit: record.limit,
                        });
                    }
                    put(&mut staged, quota_key, &record)?;
                }
                if !page.more {
                    break;
                }
                window.start = Bound::Excluded(
                    page.records
                        .last()
                        .ok_or_else(|| {
                            ArgosError::CorruptedMetadata("V3 quota scan made no progress".into())
                        })?
                        .0
                        .clone(),
                );
            }
        }
        // No partial overlay updates survive a failed quota check.
        self.changes.extend(staged);
        Ok(())
    }
    /// Replace a block and its member references atomically. The old
    /// mapping is checked against the generation, including repeated edits.
    pub fn set_block(
        &mut self,
        ino: u64,
        offset: u64,
        old: Option<&FileBlock>,
        new: Option<&FileBlock>,
    ) -> Result<()> {
        if ino == 0
            || old.into_iter().chain(new).any(|block| {
                block.raw_offset != offset
                    || block.raw_size == 0
                    || block
                        .raw_offset
                        .checked_add(block.raw_size as u64)
                        .is_none()
            })
        {
            return Err(ArgosError::Invalid("invalid V3 block range".into()));
        }
        let key = Domain::Block.inode_offset(ino, offset);
        if let Some(current) = self.changes.get(&key) {
            let current = current
                .as_deref()
                .map(decode_record::<FileBlock>)
                .transpose()?;
            if current.as_ref() != old {
                return Err(ArgosError::Conflict("V3 block overlay changed".into()));
            }
        } else {
            self.block_preconditions.insert(key.clone(), old.cloned());
        }
        let old_keys = old
            .map(|block| member_block_keys(ino, block))
            .transpose()?
            .unwrap_or_default();
        let new_keys = new
            .map(|block| member_block_keys(ino, block))
            .transpose()?
            .unwrap_or_default();
        // Compute all encoded values before modifying the overlay.
        let encoded = new.map(encode_record).transpose()?;
        let reference = new
            .map(|block| encode_record(&block.stripe_id))
            .transpose()?;
        for key in old_keys.difference(&new_keys) {
            self.delete(key.clone());
        }
        for key in new_keys {
            self.changes.insert(key, reference.clone());
        }
        self.changes.insert(key, encoded);
        Ok(())
    }
    /// Caller supplies the old directory entry from this transaction's
    /// snapshot. Reverse parent index changes share the publication.
    pub fn set_dirent(
        &mut self,
        parent: u64,
        name: &[u8],
        old: Option<u64>,
        new: Option<u64>,
    ) -> Result<()> {
        if name.is_empty()
            || name.len() > 255
            || name == b"."
            || name == b".."
            || name.contains(&0)
            || name.contains(&b'/')
            || parent == 0
            || old == Some(0)
            || new == Some(0)
        {
            return Err(ArgosError::Invalid("invalid V3 directory entry".into()));
        }
        let key = Domain::Dirent.inode_name(parent, name)?;
        if let Some(current) = self.changes.get(&key) {
            let current = current
                .as_ref()
                .map(|bytes| decode_record::<u64>(bytes))
                .transpose()?;
            if current != old {
                return Err(ArgosError::Conflict("V3 directory overlay changed".into()));
            }
        } else {
            self.dirent_preconditions.insert(key.clone(), old);
        }
        if let Some(old) = old {
            self.delete(Domain::parent(old, parent, name)?);
        }
        if let Some(new) = new {
            self.put(Domain::parent(new, parent, name)?, &())?;
            self.put(key, &new)?;
        } else {
            self.delete(key);
        }
        Ok(())
    }
    pub fn commit(self, tree: &mut super::TypedMetadata) -> Result<super::CommitReport> {
        self.check_generation(tree)?;
        for (key, expected) in self.dirent_preconditions {
            let actual = tree
                .get(key)?
                .as_deref()
                .map(decode_record::<u64>)
                .transpose()?;
            if actual != expected {
                return Err(ArgosError::Conflict("V3 directory entry changed".into()));
            }
        }
        for (key, expected) in self.block_preconditions {
            let actual = tree
                .get(key)?
                .as_deref()
                .map(decode_record::<FileBlock>)
                .transpose()?;
            if actual != expected {
                return Err(ArgosError::Conflict("V3 block mapping changed".into()));
            }
        }
        tree.commit(&self.changes)
    }
}

/// A read transaction borrows a fixed certified generation. It supports
/// typed lookups without materializing an inode table or a logical key map.
pub trait RecordRead {
    fn get_record(&self, key: TypedKey) -> Result<Option<Vec<u8>>>;
    fn scan_records(&self, window: &ScanWindow<TypedKey>) -> Result<ScanPage<TypedKey>>;
}

impl RecordRead for TypedMetadata {
    fn get_record(&self, key: TypedKey) -> Result<Option<Vec<u8>>> {
        self.get(key)
    }
    fn scan_records(&self, window: &ScanWindow<TypedKey>) -> Result<ScanPage<TypedKey>> {
        self.scan(window)
    }
}

impl RecordRead for CertifiedReader<TypedKey> {
    fn get_record(&self, key: TypedKey) -> Result<Option<Vec<u8>>> {
        self.get(key)
    }
    fn scan_records(&self, window: &ScanWindow<TypedKey>) -> Result<ScanPage<TypedKey>> {
        self.scan(window)
    }
}

pub type DirectoryPage = (Vec<(Vec<u8>, u64)>, bool);

pub struct RecordView<'a, R: RecordRead + ?Sized> {
    source: &'a R,
}

impl<'a, R: RecordRead + ?Sized> RecordView<'a, R> {
    pub fn new(source: &'a R) -> Self {
        Self { source }
    }

    fn read<T: DeserializeOwned>(&self, key: TypedKey) -> Result<Option<T>> {
        self.source
            .get_record(key)?
            .as_deref()
            .map(decode_record)
            .transpose()
    }

    pub fn statistics(&self) -> Result<Option<VolumeStatistics>> {
        self.read(Domain::Statistics.prefix())
    }

    pub fn quota(&self, ino: u64) -> Result<Option<QuotaRecord>> {
        self.read(Domain::Quota.inode_prefix(ino))
    }

    pub fn quota_membership(&self, ino: u64, quota: u64) -> Result<Option<u64>> {
        self.read(Domain::QuotaMembership.inode_offset(ino, quota))
    }

    pub fn member_reference_page(
        &self,
        id: &str,
        after: Option<(u64, u64)>,
        max_records: usize,
        max_bytes: usize,
    ) -> Result<ScanPage<TypedKey>> {
        let prefix = Domain::MemberReference.disk(id, None)?;
        self.source.scan_records(&ScanWindow {
            start: match after {
                Some((ino, offset)) => Bound::Excluded(Domain::member_block(id, ino, offset)?),
                None => Bound::Included(prefix.clone()),
            },
            end: prefix
                .prefix_end()
                .map_or(Bound::Unbounded, Bound::Excluded),
            max_records,
            max_bytes,
            order: ScanOrder::Ascending,
        })
    }

    pub fn inode(&self, ino: u64) -> Result<Option<InodeCorePage>> {
        let record: Option<InodeCorePage> = self.read(Domain::Inode.inode_prefix(ino))?;
        if record.as_ref().is_some_and(|record| record.id != ino) {
            return Err(ArgosError::CorruptedMetadata(
                "V3 inode key/value mismatch".into(),
            ));
        }
        Ok(record)
    }

    pub fn lookup(&self, parent: u64, name: &[u8]) -> Result<Option<u64>> {
        self.read(Domain::Dirent.inode_name(parent, name)?)
    }

    pub fn directory_page(
        &self,
        parent: u64,
        after: Option<&[u8]>,
        max_records: usize,
        max_bytes: usize,
    ) -> Result<DirectoryPage> {
        let prefix = Domain::Dirent.inode_prefix(parent);
        let window = ScanWindow {
            start: match after {
                Some(name) => Bound::Excluded(Domain::Dirent.inode_name(parent, name)?),
                None => Bound::Included(prefix.clone()),
            },
            end: prefix
                .prefix_end()
                .map_or(Bound::Unbounded, Bound::Excluded),
            max_records,
            max_bytes,
            order: ScanOrder::Ascending,
        };
        let page = self.source.scan_records(&window)?;
        let records = page
            .records
            .into_iter()
            .map(|(key, value)| {
                let name = key
                    .as_bytes()
                    .strip_prefix(prefix.as_bytes())
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        ArgosError::CorruptedMetadata("V3 dirent prefix mismatch".into())
                    })?;
                Ok((name.to_vec(), decode_record::<u64>(&value)?))
            })
            .collect::<Result<_>>()?;
        Ok((records, page.more))
    }

    /// Find the predecessor extent containing the requested logical offset.
    /// Holes are returned as None, not a neighboring extent.
    pub fn block_at(&self, ino: u64, offset: u64) -> Result<Option<FileBlock>> {
        let prefix = Domain::Block.inode_prefix(ino);
        let window = ScanWindow {
            start: Bound::Included(prefix.clone()),
            end: Bound::Included(Domain::Block.inode_offset(ino, offset)),
            max_records: 1,
            max_bytes: MAX_RECORD + HEADER + TypedKey::MAX_LEN,
            order: ScanOrder::Descending,
        };
        let Some((key, value)) = self.source.scan_records(&window)?.records.pop() else {
            return Ok(None);
        };
        let block: FileBlock = decode_record(&value)?;
        if key != Domain::Block.inode_offset(ino, block.raw_offset) {
            return Err(ArgosError::CorruptedMetadata(
                "V3 block key/value mismatch".into(),
            ));
        }
        let end = block
            .raw_offset
            .checked_add(block.raw_size as u64)
            .ok_or_else(|| ArgosError::CorruptedMetadata("V3 block range overflow".into()))?;
        Ok((offset >= block.raw_offset && offset < end).then_some(block))
    }

    pub fn xattr(&self, ino: u64, name: &[u8]) -> Result<Option<String>> {
        self.read(Domain::Xattr.inode_name(ino, name)?)
    }

    pub fn disk(&self, id: &str) -> Result<Option<crate::types::Disk>> {
        self.read(Domain::Disk.disk(id, None)?)
    }

    pub fn allocator_core(&self, id: &str) -> Result<Option<RawAllocatorState>> {
        self.read(Domain::DataAllocator.disk(id, None)?)
    }
}
