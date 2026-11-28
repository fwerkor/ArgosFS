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
use serde::{de::DeserializeOwned, Serialize};
use std::collections::BTreeMap;
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
    let mut changes = TypedChanges::new();
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
        put(
            &mut changes,
            Domain::Inode.inode_prefix(ino),
            &InodeCorePage::from(inode),
        )?;
        if inode.kind == crate::types::NodeKind::File {
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
        }
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
}

impl RecordTransaction {
    pub fn begin(tree: &TypedMetadata) -> Self {
        Self {
            expected: (tree.txid(), tree.root_hash()),
            namespace: tree.namespace,
            changes: TypedChanges::new(),
            dirent_preconditions: BTreeMap::new(),
        }
    }
    pub fn put(&mut self, key: TypedKey, value: &impl Serialize) -> Result<()> {
        self.changes.insert(key, Some(encode_record(value)?));
        Ok(())
    }
    pub fn delete(&mut self, key: TypedKey) {
        self.changes.insert(key, None);
    }
    pub fn put_inode(&mut self, inode: &InodeCorePage) -> Result<()> {
        self.put(Domain::Inode.inode_prefix(inode.id), inode)
    }
    pub fn put_block(&mut self, ino: u64, block: &FileBlock) -> Result<()> {
        self.put(Domain::Block.inode_offset(ino, block.raw_offset), block)
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
        if (tree.txid(), tree.root_hash()) != self.expected || tree.namespace != self.namespace {
            return Err(ArgosError::Conflict("stale V3 metadata transaction".into()));
        }
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
