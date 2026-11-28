//! Fixed-size authenticated pages. The logical digest excludes physical page
//! numbers so replicas with different allocation histories can vote together.
use crate::error::{ArgosError, Result};
use sha2::{Digest, Sha256};

pub(super) const PAGE_SIZE: usize = 4096;
const TRAILER: usize = PAGE_SIZE - 32;
const HEADER: usize = 16;
pub(super) const NODE_CAPACITY: usize = TRAILER - HEADER;
const MAGIC: &[u8; 8] = b"ARGPG001";
const ROOT_MAGIC: &[u8; 8] = b"ARGRT001";
const CERT_MAGIC: &[u8; 8] = b"ARGCT001";
pub(super) const RESERVED_PAGES: u64 = 4;
pub(super) const INLINE_MAX: usize = 768;
pub(super) const BLOB_MAX: usize = TRAILER - HEADER - 40;
pub(super) const MAX_LEVEL: u8 = 32;
pub(super) const MAX_VALUE: usize = 1024 * 1024 * 1024;
pub(super) type Hash = [u8; 32];
pub(super) type Bytes = [u8; PAGE_SIZE];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Ptr {
    pub(super) page: u64,
    pub(super) hash: Hash,
}

#[derive(Clone, Debug)]
pub(super) enum Value {
    Inline(Vec<u8>),
    Blob { len: usize, first: Ptr },
}

#[derive(Clone, Debug)]
pub(super) enum Node {
    Leaf(Vec<(u64, Value)>),
    Branch {
        level: u8,
        children: Vec<(u64, Ptr)>,
    },
    Blob {
        next: Option<Ptr>,
        data: Vec<u8>,
    },
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
        12 + match self {
            Self::Inline(bytes) => bytes.len(),
            Self::Blob { .. } => 40,
        }
    }
}

impl Node {
    pub(super) fn level(&self) -> Option<u8> {
        match self {
            Self::Leaf(_) => Some(0),
            Self::Branch { level, .. } => Some(*level),
            Self::Blob { .. } => None,
        }
    }
    pub(super) fn max_key(&self) -> Option<u64> {
        match self {
            Self::Leaf(entries) => entries.last().map(|(key, _)| *key),
            Self::Branch { children, .. } => children.last().map(|(key, _)| *key),
            Self::Blob { .. } => None,
        }
    }
    pub(super) fn children(&self) -> Vec<Ptr> {
        match self {
            Self::Leaf(entries) => entries
                .iter()
                .filter_map(|(_, value)| match value {
                    Value::Blob { first, .. } => Some(*first),
                    Value::Inline(_) => None,
                })
                .collect(),
            Self::Branch { children, .. } => children.iter().map(|(_, ptr)| *ptr).collect(),
            Self::Blob { next, .. } => next.iter().copied().collect(),
        }
    }
    pub(super) fn used(&self) -> usize {
        match self {
            Self::Leaf(entries) => entries.iter().map(|(_, v)| v.encoded_len()).sum(),
            Self::Branch { children, .. } => children.len() * 48,
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
                let total: usize = entries.iter().map(|(_, v)| v.encoded_len()).sum();
                let mut left = 0;
                let mut index = 0;
                while index + 1 < entries.len() && left < total / 2 {
                    left += entries[index].1.encoded_len();
                    index += 1;
                }
                let right = entries.split_off(index);
                Ok((Self::Leaf(entries), Self::Leaf(right)))
            }
            Self::Branch {
                level,
                mut children,
            } if children.len() > 1 => {
                let right = children.split_off(children.len() / 2);
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
                    payload.extend_from_slice(&key.to_le_bytes());
                    match value {
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
                    payload.extend_from_slice(&key.to_le_bytes());
                    write_ptr(&mut payload, Some(*ptr), logical);
                }
                (2, *level, children.len())
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
        out[..8].copy_from_slice(MAGIC);
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
        if &bytes[..8] != MAGIC || hash(&bytes[..TRAILER]) != bytes[TRAILER..] {
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
            1 if level == 0 && count > 0 && count <= len / 12 => {
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let key = input.u64()?;
                    let size = input.u32()?;
                    let value = if size & (1 << 31) != 0 {
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
            2 if level > 0 && level <= MAX_LEVEL && count > 0 && count * 48 == len => {
                let mut children = Vec::with_capacity(count);
                for _ in 0..count {
                    children.push((input.u64()?, input.required_ptr()?));
                }
                Self::Branch { level, children }
            }
            3 if level == 0 && count == 0 => {
                let next = input.ptr()?;
                let data = input.take(input.0.len())?.to_vec();
                Self::Blob { next, data }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Root {
    pub(super) txid: u64,
    pub(super) tree: Option<Ptr>,
}

impl Root {
    pub(super) fn identity(self) -> (u64, Hash) {
        (self.txid, self.tree.map_or([0; 32], |ptr| ptr.hash))
    }
    pub(super) fn encode(self, namespace: Hash, pages: u64, certificate: bool) -> Bytes {
        let mut out = [0; PAGE_SIZE];
        out[..8].copy_from_slice(if certificate { CERT_MAGIC } else { ROOT_MAGIC });
        out[8..40].copy_from_slice(&namespace);
        out[40..48].copy_from_slice(&pages.to_le_bytes());
        out[48..56].copy_from_slice(&self.txid.to_le_bytes());
        let mut ptr = Vec::new();
        write_ptr(&mut ptr, self.tree, false);
        out[56..96].copy_from_slice(&ptr);
        let checksum = hash(&out[..TRAILER]);
        out[TRAILER..].copy_from_slice(&checksum);
        out
    }

    pub(super) fn decode(
        bytes: &Bytes,
        namespace: Hash,
        pages: u64,
        certificate: bool,
    ) -> Result<Self> {
        let magic = if certificate { CERT_MAGIC } else { ROOT_MAGIC };
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
        if bytes[96..TRAILER].iter().any(|b| *b != 0) {
            return Err(corrupt("invalid metadata root padding"));
        }
        Ok(Self { txid, tree })
    }
}
