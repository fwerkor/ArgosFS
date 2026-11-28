//! Fixed-size authenticated pages. The logical digest excludes physical page
//! numbers so replicas with different allocation histories can vote together.
use crate::error::{ArgosError, Result};
use sha2::{Digest, Sha256};

pub(super) const PAGE_SIZE: usize = 4096;
const TRAILER: usize = PAGE_SIZE - 32;
const HEADER: usize = 16;
pub(super) const NODE_CAPACITY: usize = TRAILER - HEADER;
pub(super) const RESERVED_PAGES: u64 = 4;
pub(super) const INLINE_MAX: usize = 768;
pub(super) const BLOB_MAX: usize = TRAILER - HEADER - 40;
pub(super) const MAX_LEVEL: u8 = 32;
pub(super) const MAX_VALUE: usize = 1024 * 1024 * 1024;
pub(super) type Hash = [u8; 32];
pub(super) type Bytes = [u8; PAGE_SIZE];

/// The legacy numeric codec and the new ordered compound-key codec share
/// the COW machinery, but deliberately have disjoint wire magics.
pub trait TreeKey: sealed::Sealed + Clone + Ord + std::fmt::Debug + Send + Sync {
    const PAGE_MAGIC: &'static [u8; 8];
    const ROOT_MAGIC: &'static [u8; 8];
    const CERT_MAGIC: &'static [u8; 8];
    const MIN_ENCODED_LEN: usize;
    const PERSISTENT_ALLOCATOR: bool;
    fn encoded_len(&self) -> usize;
    fn domain_id(&self) -> Option<u16> {
        None
    }
    fn encode_key(&self, out: &mut Vec<u8>);
    fn decode_key(input: &mut &[u8]) -> Result<Self>;
}
mod sealed {
    pub trait Sealed {}
    impl Sealed for u64 {}
    impl Sealed for super::TypedKey {}
}
impl TreeKey for u64 {
    const PAGE_MAGIC: &'static [u8; 8] = b"ARGPG001";
    const ROOT_MAGIC: &'static [u8; 8] = b"ARGRT001";
    const CERT_MAGIC: &'static [u8; 8] = b"ARGCT001";
    const MIN_ENCODED_LEN: usize = 8;
    const PERSISTENT_ALLOCATOR: bool = false;
    fn encoded_len(&self) -> usize {
        8
    }
    fn encode_key(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes());
    }
    fn decode_key(input: &mut &[u8]) -> Result<Self> {
        let mut reader = Reader(input);
        let key = reader.u64()?;
        *input = reader.0;
        Ok(key)
    }
}

/// A lexicographic, length-delimited key. The first two bytes are the
/// big-endian namespace ID. Binary names (including non-UTF8) are lossless.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TypedKey(Vec<u8>);

impl TypedKey {
    pub const MAX_LEN: usize = 1024;

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        if !(2..=Self::MAX_LEN).contains(&bytes.len()) {
            return Err(ArgosError::Invalid("compound metadata key length".into()));
        }
        Ok(Self(bytes.into_boxed_slice().into_vec()))
    }

    pub fn new(domain: u16, suffix: &[u8]) -> Result<Self> {
        if suffix.len() > Self::MAX_LEN - 2 {
            return Err(ArgosError::Invalid("compound metadata key length".into()));
        }
        let mut bytes = domain.to_be_bytes().to_vec();
        bytes.extend_from_slice(suffix);
        Self::from_bytes(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn domain(&self) -> u16 {
        u16::from_be_bytes(self.0[..2].try_into().unwrap())
    }

    /// Exclusive upper bound of a prefix, or None for the maximal prefix.
    pub fn prefix_end(&self) -> Option<Self> {
        let mut bytes = self.0.clone();
        for pos in (0..bytes.len()).rev() {
            if bytes[pos] != u8::MAX {
                bytes[pos] += 1;
                bytes.truncate((pos + 1).max(2));
                if pos == 0 {
                    bytes[1] = 0;
                }
                return Some(Self(bytes));
            }
        }
        None
    }
}

impl TreeKey for TypedKey {
    const PAGE_MAGIC: &'static [u8; 8] = b"ARGPG003";
    const ROOT_MAGIC: &'static [u8; 8] = b"ARGRT003";
    const CERT_MAGIC: &'static [u8; 8] = b"ARGCT003";
    const MIN_ENCODED_LEN: usize = 4;
    const PERSISTENT_ALLOCATOR: bool = true;
    fn domain_id(&self) -> Option<u16> {
        Some(self.domain())
    }
    fn encoded_len(&self) -> usize {
        2 + self.0.len()
    }
    fn encode_key(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.0.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.0);
    }
    fn decode_key(input: &mut &[u8]) -> Result<Self> {
        let mut reader = Reader(input);
        let len = u16::from_le_bytes(reader.take(2)?.try_into().unwrap()) as usize;
        if !(2..=Self::MAX_LEN).contains(&len) {
            return Err(corrupt("invalid compound metadata key length"));
        }
        let key = Self(reader.take(len)?.to_vec());
        *input = reader.0;
        Ok(key)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Ptr {
    pub(super) page: u64,
    pub(super) hash: Hash,
}

#[derive(Clone, Debug)]
pub(super) enum Value {
    Inline(Vec<u8>),
    Blob { len: usize, first: Ptr },
    Snapshot { txid: u64, root: Ptr },
}

#[derive(Clone, Debug)]
pub(super) enum Node<K: TreeKey> {
    Leaf(Vec<(K, Value)>),
    Catalog(Vec<(u16, Ptr)>),
    Branch { level: u8, children: Vec<(K, Ptr)> },
    Blob { next: Option<Ptr>, data: Vec<u8> },
}

pub(super) fn corrupt(message: impl Into<String>) -> ArgosError {
    ArgosError::CorruptedMetadata(message.into())
}

fn hash(bytes: &[u8]) -> Hash {
    Sha256::digest(bytes).into()
}

fn write_ptr(out: &mut Vec<u8>, ptr: Option<Ptr>, logical: bool) {
    let (page, digest) = ptr.map(|p| (p.page, p.hash)).unwrap_or_default();
    out.extend_from_slice(&if logical { 0 } else { page }.to_le_bytes());
    out.extend_from_slice(&digest);
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let (head, tail) = self
            .0
            .split_at_checked(len)
            .ok_or_else(|| corrupt("truncated metadata page"))?;
        self.0 = tail;
        Ok(head)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn ptr(&mut self) -> Result<Option<Ptr>> {
        let page = self.u64()?;
        let hash: Hash = self.take(32)?.try_into().unwrap();
        if page == 0 {
            if hash != [0; 32] {
                return Err(corrupt("null metadata pointer has a digest"));
            }
            Ok(None)
        } else if page < RESERVED_PAGES {
            Err(corrupt("metadata pointer refers to root slot"))
        } else {
            Ok(Some(Ptr { page, hash }))
        }
    }
    fn required_ptr(&mut self) -> Result<Ptr> {
        self.ptr()?.ok_or_else(|| corrupt("missing metadata child"))
    }
}

impl Value {
    pub(super) fn encoded_len(&self) -> usize {
        4 + match self {
            Self::Inline(bytes) => bytes.len(),
            Self::Blob { .. } => 40,
            Self::Snapshot { .. } => 48,
        }
    }
}

impl<K: TreeKey> Node<K> {
    pub(super) fn level(&self) -> Option<u8> {
        match self {
            Self::Leaf(_) => Some(0),
            Self::Branch { level, .. } => Some(*level),
            Self::Blob { .. } | Self::Catalog(_) => None,
        }
    }
    pub(super) fn max_key(&self) -> Option<K> {
        match self {
            Self::Leaf(entries) => entries.last().map(|(key, _)| key.clone()),
            Self::Branch { children, .. } => children.last().map(|(key, _)| key.clone()),
            Self::Blob { .. } | Self::Catalog(_) => None,
        }
    }
    pub(super) fn children(&self) -> Vec<Ptr> {
        match self {
            Self::Leaf(entries) => entries
                .iter()
                .filter_map(|(_, value)| match value {
                    Value::Blob { first, .. } => Some(*first),
                    Value::Inline(_) => None,
                    Value::Snapshot { root, .. } => Some(*root),
                })
                .collect(),
            Self::Branch { children, .. } => children.iter().map(|(_, ptr)| *ptr).collect(),
            Self::Catalog(roots) => roots.iter().map(|(_, ptr)| *ptr).collect(),
            Self::Blob { next, .. } => next.iter().copied().collect(),
        }
    }
    pub(super) fn used(&self) -> usize {
        match self {
            Self::Leaf(entries) => entries
                .iter()
                .map(|(k, v)| k.encoded_len() + v.encoded_len())
                .sum(),
            Self::Branch { children, .. } => {
                children.iter().map(|(k, _)| k.encoded_len() + 40).sum()
            }
            Self::Catalog(roots) => roots.len() * 42,
            Self::Blob { data, .. } => 40 + data.len(),
        }
    }

    pub(super) fn fits(&self) -> bool {
        self.used() <= NODE_CAPACITY
    }

    pub(super) fn combine(self, other: Self) -> Result<Self> {
        match (self, other) {
            (Self::Leaf(mut left), Self::Leaf(right)) => {
                left.extend(right);
                Ok(Self::Leaf(left))
            }
            (
                Self::Branch {
                    level,
                    mut children,
                },
                Self::Branch {
                    level: other,
                    children: right,
                },
            ) if level == other => {
                children.extend(right);
                Ok(Self::Branch { level, children })
            }
            _ => Err(corrupt("metadata siblings have different types or levels")),
        }
    }

    pub(super) fn split(self) -> Result<(Self, Self)> {
        match self {
            Self::Leaf(mut entries) if entries.len() > 1 => {
                let total: usize = entries
                    .iter()
                    .map(|(k, v)| k.encoded_len() + v.encoded_len())
                    .sum();
                let mut left = 0;
                let mut index = 0;
                while index + 1 < entries.len() && left < total / 2 {
                    let next =
                        left + entries[index].0.encoded_len() + entries[index].1.encoded_len();
                    if K::PERSISTENT_ALLOCATOR
                        && index > 0
                        && next.abs_diff(total / 2) >= left.abs_diff(total / 2)
                    {
                        break;
                    }
                    left = next;
                    index += 1;
                }
                let right = entries.split_off(index);
                Ok((Self::Leaf(entries), Self::Leaf(right)))
            }
            Self::Branch {
                level,
                mut children,
            } if children.len() > 1 => {
                let total: usize = children.iter().map(|(k, _)| k.encoded_len() + 40).sum();
                let mut left = 0;
                let mut index = 0;
                for (key, _) in children.iter().take(children.len() - 1) {
                    let next = left + key.encoded_len() + 40;
                    if index > 0 && next.abs_diff(total / 2) >= left.abs_diff(total / 2) {
                        break;
                    }
                    left = next;
                    index += 1;
                }
                let right = children.split_off(index);
                Ok((
                    Self::Branch { level, children },
                    Self::Branch {
                        level,
                        children: right,
                    },
                ))
            }
            _ => Err(corrupt("unsplittable metadata page")),
        }
    }
    fn encode_body(&self, logical: bool) -> Result<Bytes> {
        if !self.fits() {
            return Err(corrupt("metadata page exceeds capacity"));
        }
        let mut payload = Vec::new();
        let (kind, level, count) = match self {
            Self::Leaf(entries) => {
                if entries.is_empty() || !entries.windows(2).all(|p| p[0].0 < p[1].0) {
                    return Err(corrupt("invalid metadata leaf keys"));
                }
                for (key, value) in entries {
                    key.encode_key(&mut payload);
                    if (key.domain_id() == Some(super::SNAPSHOT_DOMAIN))
                        != matches!(value, Value::Snapshot { .. })
                    {
                        return Err(corrupt("snapshot value outside reserved snapshot domain"));
                    }
                    match value {
                        Value::Snapshot { txid, root } => {
                            payload.extend_from_slice(&u32::MAX.to_le_bytes());
                            payload.extend_from_slice(&txid.to_le_bytes());
                            write_ptr(&mut payload, Some(*root), logical);
                        }
                        Value::Inline(bytes) => {
                            if bytes.len() > INLINE_MAX {
                                return Err(corrupt("oversized inline metadata value"));
                            }
                            payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                            payload.extend_from_slice(bytes);
                        }
                        Value::Blob { len, first } => {
                            if *len <= INLINE_MAX || *len > MAX_VALUE {
                                return Err(corrupt("invalid metadata blob length"));
                            }
                            payload.extend_from_slice(&((*len as u32) | (1 << 31)).to_le_bytes());
                            write_ptr(&mut payload, Some(*first), logical);
                        }
                    }
                }
                (1, 0, entries.len())
            }
            Self::Branch { level, children } => {
                if *level == 0
                    || *level > MAX_LEVEL
                    || children.is_empty()
                    || !children.windows(2).all(|p| p[0].0 < p[1].0)
                {
                    return Err(corrupt("invalid metadata branch"));
                }
                for (key, ptr) in children {
                    key.encode_key(&mut payload);
                    write_ptr(&mut payload, Some(*ptr), logical);
                }
                (2, *level, children.len())
            }
            Self::Catalog(roots) => {
                if !K::PERSISTENT_ALLOCATOR
                    || roots.is_empty()
                    || !roots.windows(2).all(|pair| pair[0].0 < pair[1].0)
                {
                    return Err(corrupt("invalid metadata root catalog"));
                }
                for (domain, ptr) in roots {
                    payload.extend_from_slice(&domain.to_be_bytes());
                    write_ptr(&mut payload, Some(*ptr), logical);
                }
                (4, 0, roots.len())
            }
            Self::Blob { next, data } => {
                if data.is_empty() || data.len() > BLOB_MAX {
                    return Err(corrupt("invalid metadata blob page"));
                }
                write_ptr(&mut payload, *next, logical);
                payload.extend_from_slice(data);
                (3, 0, 0)
            }
        };
        let mut out = [0; PAGE_SIZE];
        out[..8].copy_from_slice(K::PAGE_MAGIC);
        out[8] = kind;
        out[9] = level;
        out[10..12].copy_from_slice(&(count as u16).to_le_bytes());
        out[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        out[HEADER..HEADER + payload.len()].copy_from_slice(&payload);
        Ok(out)
    }
    pub(super) fn encode(&self) -> Result<(Bytes, Hash)> {
        let mut out = self.encode_body(false)?;
        let physical = hash(&out[..TRAILER]);
        out[TRAILER..].copy_from_slice(&physical);
        let logical = hash(&self.encode_body(true)?[..TRAILER]);
        Ok((out, logical))
    }
    pub(super) fn decode(bytes: &Bytes, expected: Hash) -> Result<Self> {
        if &bytes[..8] != K::PAGE_MAGIC || hash(&bytes[..TRAILER]) != bytes[TRAILER..] {
            return Err(ArgosError::Checksum(
                "metadata page physical checksum mismatch".into(),
            ));
        }
        let level = bytes[9];
        let count = u16::from_le_bytes(bytes[10..12].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        if len > TRAILER - HEADER || bytes[HEADER + len..TRAILER].iter().any(|b| *b != 0) {
            return Err(corrupt("invalid metadata page length or padding"));
        }
        let mut input = Reader(&bytes[HEADER..HEADER + len]);
        let node = match bytes[8] {
            1 if level == 0 && count > 0 && count <= len / (K::MIN_ENCODED_LEN + 4) => {
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let key = K::decode_key(&mut input.0)?;
                    let size = input.u32()?;
                    let value = if size == u32::MAX && K::PERSISTENT_ALLOCATOR {
                        Value::Snapshot {
                            txid: input.u64()?,
                            root: input.required_ptr()?,
                        }
                    } else if size & (1 << 31) != 0 {
                        let len = (size & !(1 << 31)) as usize;
                        Value::Blob {
                            len,
                            first: input.required_ptr()?,
                        }
                    } else {
                        Value::Inline(input.take(size as usize)?.to_vec())
                    };
                    entries.push((key, value));
                }
                Self::Leaf(entries)
            }
            2 if level > 0
                && level <= MAX_LEVEL
                && count > 0
                && count <= len / (K::MIN_ENCODED_LEN + 40) =>
            {
                let mut children = Vec::with_capacity(count);
                for _ in 0..count {
                    children.push((K::decode_key(&mut input.0)?, input.required_ptr()?));
                }
                Self::Branch { level, children }
            }
            3 if level == 0 && count == 0 => {
                let next = input.ptr()?;
                let data = input.take(input.0.len())?.to_vec();
                Self::Blob { next, data }
            }
            4 if K::PERSISTENT_ALLOCATOR && level == 0 && count > 0 && count * 42 == len => {
                let mut roots = Vec::with_capacity(count);
                for _ in 0..count {
                    let domain = u16::from_be_bytes(input.take(2)?.try_into().unwrap());
                    roots.push((domain, input.required_ptr()?));
                }
                Self::Catalog(roots)
            }
            _ => return Err(corrupt("unknown or malformed metadata page type")),
        };
        if !input.0.is_empty() || hash(&node.encode_body(true)?[..TRAILER]) != expected {
            return Err(ArgosError::Checksum(
                "metadata page Merkle digest mismatch".into(),
            ));
        }
        Ok(node)
    }
}

/// Feature negotiation is part of the logical quorum identity. An unknown
/// compatible feature is preserved; unknown read-only-compatible features
/// prevent writes; unknown incompatible features prevent all access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct V3Features {
    pub compat: u64,
    pub ro_compat: u64,
    pub incompat: u64,
}

impl Default for V3Features {
    fn default() -> Self {
        Self {
            compat: 0,
            ro_compat: 0,
            incompat: Self::REQUIRED,
        }
    }
}

impl V3Features {
    // Bit 3 is reserved for the final logical object/ownership format.
    // Bit 63 marks this incompatible, unshipped extent-pin prototype.
    const REQUIRED: u64 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 63);

    pub fn validate(self, writable: bool) -> Result<()> {
        if self.incompat != Self::REQUIRED {
            return Err(ArgosError::IncompatibleFormat(format!(
                "unsupported V3 incompatible features: {:#x}",
                self.incompat
            )));
        }
        if writable && self.ro_compat != 0 {
            return Err(ArgosError::ReadonlyRequired(format!(
                "unsupported V3 read-only features: {:#x}",
                self.ro_compat
            )));
        }
        Ok(())
    }

    fn bytes(self) -> [u8; 24] {
        let mut bytes = [0; 24];
        bytes[..8].copy_from_slice(&self.compat.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.ro_compat.to_le_bytes());
        bytes[16..].copy_from_slice(&self.incompat.to_le_bytes());
        bytes
    }
}

/// Member-local state bound to a root/certificate. The allocator describes
/// the union of this logical root and the exact retained alternate root.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PhysicalState {
    pub(super) allocator: super::physical::AllocatorRoot,
    pub(super) retained: Hash,
    pub(super) features: V3Features,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Root {
    pub(super) txid: u64,
    pub(super) tree: Option<Ptr>,
    pub(super) physical: Option<PhysicalState>,
}

impl Root {
    pub(super) fn identity(self) -> (u64, Hash) {
        let tree = self.tree.map_or([0; 32], |ptr| ptr.hash);
        if let Some(physical) = self.physical {
            let mut bytes = b"ArgosFS/logical-root/v3\0".to_vec();
            bytes.extend_from_slice(&physical.features.bytes());
            bytes.extend_from_slice(&tree);
            (self.txid, hash(&bytes))
        } else {
            (self.txid, tree)
        }
    }

    pub(super) fn validate_features(self, writable: bool) -> Result<()> {
        self.physical
            .map_or(Ok(()), |physical| physical.features.validate(writable))
    }
    pub(super) fn local_identity(self) -> Hash {
        let mut bytes = b"ArgosFS/physical-root/v3\0".to_vec();
        bytes.extend_from_slice(&self.txid.to_le_bytes());
        write_ptr(&mut bytes, self.tree, false);
        if let Some(physical) = self.physical {
            bytes.extend_from_slice(&physical.allocator.encode());
            bytes.extend_from_slice(&physical.retained);
            bytes.extend_from_slice(&physical.features.bytes());
        }
        hash(&bytes)
    }

    pub(super) fn encode<K: TreeKey>(
        self,
        namespace: Hash,
        pages: u64,
        certificate: bool,
    ) -> Bytes {
        let mut out = [0; PAGE_SIZE];
        out[..8].copy_from_slice(if certificate {
            K::CERT_MAGIC
        } else {
            K::ROOT_MAGIC
        });
        out[8..40].copy_from_slice(&namespace);
        out[40..48].copy_from_slice(&pages.to_le_bytes());
        out[48..56].copy_from_slice(&self.txid.to_le_bytes());
        let mut ptr = Vec::new();
        write_ptr(&mut ptr, self.tree, false);
        out[56..96].copy_from_slice(&ptr);
        if K::PERSISTENT_ALLOCATOR {
            let physical = self
                .physical
                .expect("V3 root requires physical allocation state");
            out[96..144].copy_from_slice(&physical.allocator.encode());
            out[144..176].copy_from_slice(&physical.retained);
            out[176..178].copy_from_slice(&1u16.to_le_bytes());
            out[184..208].copy_from_slice(&physical.features.bytes());
        } else {
            debug_assert!(self.physical.is_none());
        }
        let checksum = hash(&out[..TRAILER]);
        out[TRAILER..].copy_from_slice(&checksum);
        out
    }

    pub(super) fn decode<K: TreeKey>(
        bytes: &Bytes,
        namespace: Hash,
        pages: u64,
        certificate: bool,
    ) -> Result<Self> {
        let magic = if certificate {
            K::CERT_MAGIC
        } else {
            K::ROOT_MAGIC
        };
        if &bytes[..8] != magic
            || bytes[8..40] != namespace
            || hash(&bytes[..TRAILER]) != bytes[TRAILER..]
        {
            return Err(ArgosError::Checksum(
                "metadata root checksum or identity mismatch".into(),
            ));
        }
        let mut reader = Reader(&bytes[40..96]);
        if reader.u64()? != pages {
            return Err(corrupt("metadata arena size mismatch"));
        }
        let txid = reader.u64()?;
        let tree = reader.ptr()?;
        let (physical, end) = if K::PERSISTENT_ALLOCATOR {
            let version = u16::from_le_bytes(bytes[176..178].try_into().unwrap());
            if version != 1 {
                return Err(ArgosError::IncompatibleFormat(format!(
                    "V3 root envelope version {version}"
                )));
            }
            if bytes[178..184].iter().any(|&byte| byte != 0) {
                return Err(corrupt("invalid V3 root envelope padding"));
            }
            (
                Some(PhysicalState {
                    allocator: super::physical::AllocatorRoot::decode(
                        bytes[96..144].try_into().unwrap(),
                    )?,
                    retained: bytes[144..176].try_into().unwrap(),
                    features: V3Features {
                        compat: u64::from_le_bytes(bytes[184..192].try_into().unwrap()),
                        ro_compat: u64::from_le_bytes(bytes[192..200].try_into().unwrap()),
                        incompat: u64::from_le_bytes(bytes[200..208].try_into().unwrap()),
                    },
                }),
                208,
            )
        } else {
            (None, 96)
        };
        if bytes[end..TRAILER].iter().any(|b| *b != 0) {
            return Err(corrupt("invalid metadata root padding"));
        }
        Ok(Self {
            txid,
            tree,
            physical,
        })
    }
}
